# 模型目录与角色推荐

首版目录是**版本化、只读、内置**的小目录，只列出已经验证过 Relay 路线的组合；
它不是全市场模型列表，也不代表账户登录、余额或厂商当前权益。

## 查看

```sh
relayctl="$HOME/Applications/RelayTaskWindow.app/Contents/Resources/relay/relayctl"
"$relayctl" catalog list
"$relayctl" catalog recommend
```

`relay_plan` 的 `resolved.catalog.recommendation` 也会返回同一推荐，便于预览核对。

## 当前条目

| ID | 工具 | 角色 | 能力 | 费用类别 | 生产 |
| --- | --- | --- | --- | --- | --- |
| `minimax/MiniMax-M3.1-Flash` | Claude Code | 实现 | 读/写文件 | existing_subscription | 是 |
| `opencode-go/deepseek-v4.1-flash`（别名 `deepseek-v4.1-flash`） | OpenCode Go | 只读评审 | 读文件 | existing_subscription | 是 |

评审模型可用 `RELAY_GO_MODEL` 覆盖为同一工具下你已启用的模型；approval 绑定实际模型，且覆盖值不得与实现模型相同。目录条目不因覆盖而改变。
| `offline/fake-implementer-v1` | 本地 fake | 实现 | 读/写文件 | free_tier | 否 |
| `offline/fake-reviewer-v1` | 本地 fake | 只读评审 | 读文件 | free_tier | 否 |

## 推荐规则

1. 只考虑 `verified=true` 且 `production=true` 的条目。
2. 实现角色必须声明 `edit_files`；评审只能声明 `read_files`。
3. 实现者与评审者工具不同；归一化 ID 或别名相同不算独立。
4. 枚举完整组合，按目录顺序取第一个满足条件的组合；相同输入和目录快照结果一致。
5. 缺组合时返回 `no_verified_pair` 和下一步，不把离线 fake 或同名别名当生产路线。

## 边界

- 条目只描述路线能力，不自动授权账户或消耗额度；真实任务仍需 `route-preview` 和
  用户确认。
- MiniMax 扣费归属仍为 `unverified`；OpenCode Go 仍需供应商报告的套餐来源与零余额扣费。
- 目录更新、别名重定向、型号退役和预授权备用路线仍属于后续工作；当前版本不静默
  替换固定配置。
