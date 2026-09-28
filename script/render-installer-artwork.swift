// Rebuild committed installer artwork on macOS: swift script/render-installer-artwork.swift
import AppKit
import ImageIO
import UniformTypeIdentifiers

let output = URL(fileURLWithPath: "Support/Installer", isDirectory: true)
try FileManager.default.createDirectory(at: output, withIntermediateDirectories: true)
let icon = NSImage(contentsOfFile: "Support/AppIcon.png")!
let width = 720, height = 440
func color(_ hex: UInt32) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 255) / 255,
            green: CGFloat((hex >> 8) & 255) / 255,
            blue: CGFloat(hex & 255) / 255, alpha: 1)
}
func frame(_ phase: Double) -> CGImage {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: width, pixelsHigh: height,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    color(0x18191B).setFill()
    NSRect(x: 0, y: 0, width: width, height: height).fill()
    func text(_ value: String, x: CGFloat, y: CGFloat, size: CGFloat,
              weight: NSFont.Weight = .regular, ink: UInt32 = 0xF2F2F7) {
        (value as NSString).draw(at: NSPoint(x: x, y: y), withAttributes: [
            .font: NSFont.systemFont(ofSize: size, weight: weight), .foregroundColor: color(ink)])
    }
    icon.draw(in: NSRect(x: 48, y: 314, width: 70, height: 70))
    text("Align", x: 136, y: 335, size: 32, weight: .semibold)
    text("AUDIO & VIDEO SYNC", x: 138, y: 314, size: 10, weight: .medium, ink: 0xABABB4)
    text("Everything in sync.", x: 48, y: 246, size: 34, weight: .medium)
    text("Your recordings. One timeline.", x: 49, y: 213, size: 17, ink: 0xABABB4)
    // Timeline lanes echo Align's actual blue, cyan and green clip colors.
    let hues: [UInt32] = [0x0A84FF, 0x32ADE6, 0x30D158]
    let starts: [CGFloat] = [0, 68, 32]
    for lane in 0..<3 {
        let y = CGFloat(144 - lane * 22)
        color(0x292B30).setFill()
        NSBezierPath(roundedRect: NSRect(x: 49, y: y, width: 622, height: 15), xRadius: 4, yRadius: 4).fill()
        let shift = starts[lane] * CGFloat((1 + cos(phase * 2 * .pi)) / 2)
        color(hues[lane]).withAlphaComponent(0.8).setFill()
        NSBezierPath(roundedRect: NSRect(x: 85 + shift, y: y, width: 380 - CGFloat(lane * 42), height: 15), xRadius: 4, yRadius: 4).fill()
        for tick in 0..<54 {
            let h = CGFloat(3 + (tick * 13 + lane * 7) % 8)
            color(0xFFFFFF).withAlphaComponent(0.42).setFill()
            NSRect(x: 94 + shift + CGFloat(tick * 5), y: y + (15 - h) / 2, width: 1, height: h).fill()
        }
    }
    color(0xF2F2F7).withAlphaComponent(0.7).setFill()
    NSRect(x: 85, y: 95, width: 1, height: 72).fill()
    text("Installing Align…", x: 49, y: 49, size: 14, weight: .medium)
    text("Free. Open source. Yours.", x: 500, y: 49, size: 12, ink: 0xABABB4)
    NSGraphicsContext.restoreGraphicsState()
    return bitmap.cgImage!
}
let gif = CGImageDestinationCreateWithURL(output.appendingPathComponent("Setup.gif") as CFURL,
                                         UTType.gif.identifier as CFString, 48, nil)!
CGImageDestinationSetProperties(gif, [kCGImagePropertyGIFDictionary: [kCGImagePropertyGIFLoopCount: 0]] as CFDictionary)
for index in 0..<48 {
    CGImageDestinationAddImage(gif, frame(Double(index) / 48),
        [kCGImagePropertyGIFDictionary: [kCGImagePropertyGIFDelayTime: 0.06]] as CFDictionary)
}
precondition(CGImageDestinationFinalize(gif))
let png = CGImageDestinationCreateWithURL(output.appendingPathComponent("Setup.png") as CFURL,
                                         UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(png, frame(0.5), nil)
precondition(CGImageDestinationFinalize(png))
// ICO is a container of PNG images; preserve the existing icon exactly.
var images: [Data] = []
let sizes = [16, 32, 48, 64, 128, 256]
for size in sizes {
    let bitmap = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
    NSGraphicsContext.saveGraphicsState()
    NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)
    icon.draw(in: NSRect(x: 0, y: 0, width: size, height: size))
    NSGraphicsContext.restoreGraphicsState()
    images.append(bitmap.representation(using: .png, properties: [:])!)
}
var ico = Data()
func u16(_ value: UInt16) { var n = value.littleEndian; withUnsafeBytes(of: &n) { ico.append(contentsOf: $0) } }
func u32(_ value: UInt32) { var n = value.littleEndian; withUnsafeBytes(of: &n) { ico.append(contentsOf: $0) } }
u16(0); u16(1); u16(UInt16(sizes.count))
var offset = 6 + 16 * sizes.count
for (size, data) in zip(sizes, images) {
    ico.append(contentsOf: [UInt8(size % 256), UInt8(size % 256), 0, 0])
    u16(1); u16(32); u32(UInt32(data.count)); u32(UInt32(offset))
    offset += data.count
}
for data in images { ico.append(data) }
try ico.write(to: output.appendingPathComponent("Align.ico"))
