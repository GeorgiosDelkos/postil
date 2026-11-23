// Draws Postil's app icon, assets/icon-1024.png: a black rounded square with a white "P".
// Apple's icon grid: a 1024 px canvas, an 824 px body centred, corner radius 185 px, the rest
// transparent. scripts/bundle.sh turns the PNG into the bundle's .icns.
//
// Regenerate with: swift assets/make-icon.swift assets/icon-1024.png
import AppKit

guard CommandLine.arguments.count == 2 else {
  FileHandle.standardError.write(Data("usage: swift assets/make-icon.swift <output.png>\n".utf8))
  exit(2)
}
let output = URL(fileURLWithPath: CommandLine.arguments[1])
let size = 1024

let bitmap = NSBitmapImageRep(
  bitmapDataPlanes: nil, pixelsWide: size, pixelsHigh: size, bitsPerSample: 8,
  samplesPerPixel: 4, hasAlpha: true, isPlanar: false, colorSpaceName: .deviceRGB,
  bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: bitmap)

let body = NSRect(x: 100, y: 100, width: 824, height: 824)
NSColor.black.setFill()
NSBezierPath(roundedRect: body, xRadius: 185, yRadius: 185).fill()

// The system font's bold "P", its capital height centred in the body.
let font = NSFont.systemFont(ofSize: 600, weight: .bold)
let letter = NSAttributedString(
  string: "P", attributes: [.font: font, .foregroundColor: NSColor.white])
let width = letter.size().width
let baseline = body.midY - font.capHeight / 2
letter.draw(at: NSPoint(x: body.midX - width / 2, y: baseline + font.descender))

NSGraphicsContext.restoreGraphicsState()
try! bitmap.representation(using: .png, properties: [:])!.write(to: output)
