//! Data-only implementer contract. The CLI never receives writable repository access.
use relay_git::{sha256_hex, WriteScope};
use relay_store::PlanRecord;
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet};
use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::unix::fs::{DirBuilderExt, MetadataExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

const MAX_FILES: usize = 16;
const MAX_FILE: usize = 32 * 1024;
/// Must stay below the live route's 64 KiB stage-input cap after the prompt
/// wrapper and JSON envelope are added. A larger value here would let a plan
/// preview succeed and then fail only after the worker has started.
const MAX_INPUT: usize = 60 * 1024;

pub(crate) struct EditRequest {
    root: PathBuf,
    inputs: BTreeMap<String, String>,
    pub(crate) payload: Value,
}

fn existing_file(root: &Path, name: &str) -> io::Result<(PathBuf, String, fs::Permissions)> {
    let path = root.join(name);
    if path.canonicalize()? != path {
        return Err(io::Error::other("EDIT_LINK_OR_ALIAS"));
    }
    let file = OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK)
        .open(&path)?;
    let meta = file.metadata()?;
    if !meta.is_file() || meta.nlink() != 1 || meta.len() > MAX_FILE as u64 {
        return Err(io::Error::other("EDIT_FILE_UNSUPPORTED"));
    }
    let mut data = String::new();
    file.take(MAX_FILE as u64 + 1).read_to_string(&mut data)?;
    if data.len() > MAX_FILE || data.contains('\0') {
        return Err(io::Error::other("EDIT_FILE_UNSUPPORTED"));
    }
    Ok((path, data, meta.permissions()))
}

impl EditRequest {
    pub(crate) fn new(
        worktree: &Path,
        plan: &PlanRecord,
        scope: &WriteScope,
        feedback: Option<&Value>,
    ) -> io::Result<Self> {
        let root = worktree.canonicalize()?;
        if plan.write_scope.is_empty() || plan.write_scope.len() > MAX_FILES {
            return Err(io::Error::other("EDIT_SCOPE_LIMIT"));
        }
        let mut inputs = BTreeMap::new();
        let mut files = Vec::new();
        let mut total = 0;
        for name in &plan.write_scope {
            if !scope.covers(name).unwrap_or(false) {
                return Err(io::Error::other("EDIT_EXACT_PATH_REQUIRED"));
            }
            let (_, body, _) = existing_file(&root, name)?;
            total += body.len();
            if total > MAX_INPUT {
                return Err(io::Error::other("EDIT_INPUT_LIMIT"));
            }
            let digest = sha256_hex(body.as_bytes());
            files.push(json!({"path":name,"before_sha256":digest,"text":body}));
            inputs.insert(name.clone(), digest);
        }
        let payload = json!({
            "base_oid":plan.base_oid, "goal":plan.goal, "brief":plan.brief,
            "files":files, "review_feedback":feedback,
        });
        if serde_json::to_vec(&payload)?.len() > MAX_INPUT {
            return Err(io::Error::other("EDIT_INPUT_LIMIT"));
        }
        Ok(Self {
            root,
            inputs,
            payload,
        })
    }

    pub(crate) fn apply(&self, value: &Value) -> io::Result<()> {
        let rejected = |code| io::Error::other(code);
        let map = value.as_object().ok_or_else(|| rejected("EDIT_SCHEMA"))?;
        if map.len() != 2 || value["schema_version"] != 1 {
            return Err(rejected("EDIT_SCHEMA"));
        }
        let edits = value["edits"]
            .as_array()
            .filter(|v| !v.is_empty() && v.len() <= MAX_FILES)
            .ok_or_else(|| rejected("EDIT_SCHEMA"))?;
        let mut entries = Vec::new();
        let mut seen = BTreeSet::new();
        // Validate the entire response before touching any candidate file.
        for edit in edits {
            if edit.as_object().is_none_or(|map| map.len() != 3) {
                return Err(rejected("EDIT_SCHEMA"));
            }
            let name = edit["path"]
                .as_str()
                .ok_or_else(|| rejected("EDIT_SCHEMA"))?;
            let pinned = self
                .inputs
                .get(name)
                .ok_or_else(|| rejected("EDIT_PATH_OUTSIDE_APPROVAL"))?;
            if !seen.insert(name) {
                return Err(rejected("EDIT_DUPLICATE_PATH"));
            }
            if edit["before_sha256"].as_str() != Some(pinned) {
                return Err(rejected("EDIT_BASE_MISMATCH"));
            }
            let text = edit["text"]
                .as_str()
                .filter(|s| s.len() <= MAX_FILE && !s.contains('\0'))
                .ok_or_else(|| rejected("EDIT_SCHEMA"))?;
            let (path, current, permissions) = existing_file(&self.root, name)?;
            if &sha256_hex(current.as_bytes()) != pinned {
                return Err(rejected("EDIT_BASE_MISMATCH"));
            }
            if text != current {
                entries.push((path, text.as_bytes(), permissions));
            }
        }
        if entries.is_empty() {
            return Err(rejected("EDIT_NO_CHANGE"));
        }
        // Stage every replacement before publishing any. Failures leave only an
        // unpublishable private worktree; never truncate an existing inode.
        let staged = self.root.join(".relay-edits");
        fs::DirBuilder::new().mode(0o700).create(&staged)?;
        let result = (|| {
            for (idx, (_, body, permissions)) in entries.iter().enumerate() {
                let file = staged.join(idx.to_string());
                let mut output = OpenOptions::new()
                    .create_new(true)
                    .write(true)
                    .mode(0o600)
                    .open(file)?;
                output.write_all(body)?;
                output.set_permissions(fs::Permissions::from_mode(permissions.mode() & 0o777))?;
                output.sync_all()?;
            }
            for (idx, (path, _, _)) in entries.iter().enumerate() {
                fs::rename(staged.join(idx.to_string()), path)?;
            }
            Ok(())
        })();
        let cleanup = fs::remove_dir_all(staged);
        result.and(cleanup)
    }
}
