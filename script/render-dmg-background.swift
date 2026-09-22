// Render the Finder installation background using native macOS drawing APIs.
import AppKit

let size = NSSize(width: 720, height: 440)
let image = NSImage(size: size)
image.lockFocus()
let bounds = NSRect(origin: .zero, size: size)
NSGradient(colors: [NSColor(calibratedRed: 0.94, green: 0.97, blue: 1, alpha: 1),
                    NSColor(calibratedRed: 0.99, green: 0.98, blue: 0.96, alpha: 1)])!
    .draw(in: bounds, angle: -30)
func text(_ value: String, _ y: CGFloat, _ font: NSFont, _ color: NSColor) {
    let style = NSMutableParagraphStyle()
    style.alignment = .center
    (value as NSString).draw(in: NSRect(x: 32, y: y, width: 656, height: 44),
        withAttributes: [.font: font, .foregroundColor: color, .paragraphStyle: style])
}
let ink = NSColor(calibratedRed: 0.12, green: 0.17, blue: 0.24, alpha: 1)
let secondary = NSColor(calibratedRed: 0.35, green: 0.42, blue: 0.50, alpha: 1)
text("Align", 350, .systemFont(ofSize: 34, weight: .semibold), ink)
for x: CGFloat in [190, 530] {
    NSColor.white.withAlphaComponent(0.65).setFill()
    NSBezierPath(roundedRect: NSRect(x: x - 76, y: 133, width: 152, height: 150),
                 xRadius: 28, yRadius: 28).fill()
}
let arrow = NSBezierPath()
arrow.move(to: NSPoint(x: 323, y: 210))
arrow.line(to: NSPoint(x: 397, y: 210))
arrow.move(to: NSPoint(x: 384, y: 222))
arrow.line(to: NSPoint(x: 397, y: 210))
arrow.line(to: NSPoint(x: 384, y: 198))
arrow.lineWidth = 2.5
arrow.lineCapStyle = .round
arrow.lineJoinStyle = .round
secondary.withAlphaComponent(0.65).setStroke()
arrow.stroke()
text("Drag Align to Applications", 65, .systemFont(ofSize: 17, weight: .medium), ink)
text("Free, open-source audio & video sync", 24, .systemFont(ofSize: 12), secondary)
image.unlockFocus()
let bitmap = NSBitmapImageRep(data: image.tiffRepresentation!)!
try bitmap.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: CommandLine.arguments[1]))
