// Render the vector source at each native icon size; no font or screen-scale dependency.
// Usage: swift scripts/generate-icon.swift [output-directory]
import AppKit

enum IconError: Error {
    case invalidSource, bitmap, encoding, iconutil(Int32)
}

let root = URL(fileURLWithPath: #filePath).deletingLastPathComponent().deletingLastPathComponent()
let output = CommandLine.arguments.count > 1
    ? URL(fileURLWithPath: CommandLine.arguments[1], isDirectory: true)
    : root.appendingPathComponent("assets", isDirectory: true)
let source = root.appendingPathComponent("assets/Nutty.svg")
guard let vector = NSImage(contentsOf: source) else { throw IconError.invalidSource }
let files = FileManager.default
try files.createDirectory(at: output, withIntermediateDirectories: true)
let temporary = files.temporaryDirectory.appendingPathComponent("nutty-icon-\(UUID().uuidString)", isDirectory: true)
let iconset = temporary.appendingPathComponent("Nutty.iconset", isDirectory: true)
try files.createDirectory(at: iconset, withIntermediateDirectories: true)
defer { try? files.removeItem(at: temporary) }

func png(size: Int) throws -> Data {
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    ), let context = NSGraphicsContext(bitmapImageRep: bitmap) else { throw IconError.bitmap }
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = context
    context.imageInterpolation = .high
    context.shouldAntialias = true
    vector.draw(in: NSRect(x: 0, y: 0, width: size, height: size), from: .zero, operation: .copy, fraction: 1)
    context.flushGraphics()
    guard let data = bitmap.representation(using: .png, properties: [:]) else { throw IconError.encoding }
    return data
}

for points in [16, 32, 128, 256, 512] {
    for scale in [1, 2] {
        let suffix = scale == 1 ? "" : "@2x"
        let data = try png(size: points * scale)
        try data.write(to: iconset.appendingPathComponent("icon_\(points)x\(points)\(suffix).png"))
        if points == 512 && scale == 2 {
            try data.write(to: output.appendingPathComponent("Nutty.png"), options: .atomic)
        }
        if points == 512 && scale == 1 {
            try data.write(to: output.appendingPathComponent("Nutty-workspace.png"), options: .atomic)
        }
    }
}

// PNG-backed ICO entries preserve the same artwork and alpha at every Windows DPI.
let windowsSizes = [16, 24, 32, 48, 64, 128, 256]
let windowsImages = try windowsSizes.map { try png(size: $0) }
var ico = Data()
func appendLE<T: FixedWidthInteger>(_ value: T) {
    var little = value.littleEndian
    withUnsafeBytes(of: &little) { ico.append(contentsOf: $0) }
}
appendLE(UInt16(0)); appendLE(UInt16(1)); appendLE(UInt16(windowsSizes.count))
var imageOffset = UInt32(6 + 16 * windowsSizes.count)
for (size, data) in zip(windowsSizes, windowsImages) {
    ico.append(contentsOf: [UInt8(size % 256), UInt8(size % 256), 0, 0])
    appendLE(UInt16(1)); appendLE(UInt16(32))
    appendLE(UInt32(data.count)); appendLE(imageOffset)
    imageOffset += UInt32(data.count)
}
for data in windowsImages { ico.append(data) }
try ico.write(to: output.appendingPathComponent("Nutty.ico"), options: .atomic)

let iconutil = Process()
iconutil.executableURL = URL(fileURLWithPath: "/usr/bin/iconutil")
iconutil.arguments = ["--convert", "icns", "--output", temporary.appendingPathComponent("Nutty.icns").path, iconset.path]
try iconutil.run()
iconutil.waitUntilExit()
guard iconutil.terminationStatus == 0 else { throw IconError.iconutil(iconutil.terminationStatus) }
try Data(contentsOf: temporary.appendingPathComponent("Nutty.icns"))
    .write(to: output.appendingPathComponent("Nutty.icns"), options: .atomic)

func preview() throws -> Data {
    guard let bitmap = NSBitmapImageRep(
        bitmapDataPlanes: nil, pixelsWide: 880, pixelsHigh: 480,
        bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
        colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0
    ), let context = NSGraphicsContext(bitmapImageRep: bitmap) else { throw IconError.bitmap }
    NSGraphicsContext.saveGraphicsState()
    defer { NSGraphicsContext.restoreGraphicsState() }
    NSGraphicsContext.current = context
    context.imageInterpolation = .high
    context.shouldAntialias = true
    for (origin, shade) in [(0.0, 0.95), (440.0, 0.14)] {
        NSColor(calibratedWhite: shade, alpha: 1).setFill()
        NSRect(x: origin, y: 0, width: 440, height: 480).fill()
        vector.draw(in: NSRect(x: origin + 56, y: 140, width: 328, height: 328),
                    from: .zero, operation: .sourceOver, fraction: 1)
        for (size, center) in [(16.0, 116.0), (32.0, 194.0), (64.0, 296.0)] {
            vector.draw(in: NSRect(x: origin + center - size / 2, y: 73 - size / 2,
                                  width: size, height: size),
                        from: .zero, operation: .sourceOver, fraction: 1)
        }
    }
    context.flushGraphics()
    guard let data = bitmap.representation(using: .png, properties: [:]) else { throw IconError.encoding }
    return data
}
try preview().write(to: output.appendingPathComponent("Nutty-preview.png"), options: .atomic)
print("Generated \(output.appendingPathComponent("Nutty.png").path) and Nutty.icns from Nutty.svg")
