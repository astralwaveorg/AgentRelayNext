import AppKit
import CoreGraphics

let output = URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
let rgb = CGColorSpaceCreateDeviceRGB()
func color(_ r: CGFloat, _ g: CGFloat, _ b: CGFloat, _ a: CGFloat = 1) -> CGColor {
    CGColor(red: r, green: g, blue: b, alpha: a)
}
func draw(_ size: Int) -> NSBitmapImageRep {
    let canvas = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                           bytesPerRow: 0, space: rgb, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
    let scale = CGFloat(size) / 1024
    canvas.scaleBy(x: scale, y: scale)
    let tile = CGPath(roundedRect: CGRect(x: 80, y: 80, width: 864, height: 864),
                      cornerWidth: 198, cornerHeight: 198, transform: nil)
    canvas.saveGState()
    canvas.addPath(tile)
    canvas.clip()
    let gradient = CGGradient(colorsSpace: rgb,
                              colors: [color(0.20, 0.66, 0.91), color(0.12, 0.20, 0.49), color(0.08, 0.10, 0.25)] as CFArray,
                              locations: [0, 0.60, 1])!
    canvas.drawLinearGradient(gradient, start: CGPoint(x: 185, y: 940),
                              end: CGPoint(x: 855, y: 80), options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
    canvas.restoreGState()
    canvas.addPath(tile)
    canvas.setStrokeColor(color(0.80, 0.96, 1, 0.36))
    canvas.setLineWidth(8)
    canvas.strokePath()

    // Two routes meet at a shared relay node. The high-contrast outline stays legible at 16 pt.
    let routes: [[CGPoint]] = [
        [.init(x: 293, y: 688), .init(x: 512, y: 512), .init(x: 731, y: 688)],
        [.init(x: 293, y: 336), .init(x: 512, y: 512), .init(x: 731, y: 336)],
    ]
    canvas.setLineWidth(52)
    canvas.setLineCap(.round)
    canvas.setLineJoin(.round)
    for (index, route) in routes.enumerated() {
        let tint = index == 0 ? color(0.94, 0.99, 1) : color(0.49, 0.95, 0.87)
        canvas.setStrokeColor(tint)
        canvas.addLines(between: route)
        canvas.strokePath()
        canvas.setFillColor(tint)
        for end in [route[0], route[2]] {
            canvas.fillEllipse(in: CGRect(x: end.x - 54, y: end.y - 54, width: 108, height: 108))
        }
    }
    canvas.setFillColor(color(0.96, 1, 1))
    canvas.fillEllipse(in: CGRect(x: 446, y: 446, width: 132, height: 132))
    return NSBitmapImageRep(cgImage: canvas.makeImage()!)
}

for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let suffix = scale == 1 ? "" : "@2x"
        let name = "icon_\(points)x\(points)\(suffix).png"
        try draw(points * scale).representation(using: .png, properties: [:])!.write(to: output.appendingPathComponent(name))
    }
}
