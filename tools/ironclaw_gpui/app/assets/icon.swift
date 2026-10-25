// Renders the IronClaw app icon (1024 px PNG) with CoreGraphics.
// Usage: swift icon.swift <out.png>
// Concept: three claw slashes torn through a dark forged-steel plate, molten metal glowing inside.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size = 1024.0
let cs = CGColorSpace(name: CGColorSpace.displayP3)!
let ctx = CGContext(data: nil, width: Int(size), height: Int(size), bitsPerComponent: 8,
                    bytesPerRow: 0, space: cs, bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
// Flip to top-left origin so coordinates read like a design tool.
ctx.translateBy(x: 0, y: size)
ctx.scaleBy(x: 1, y: -1)

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> CGColor {
    CGColor(colorSpace: cs, components: [CGFloat((hex >> 16) & 0xff) / 255,
                                         CGFloat((hex >> 8) & 0xff) / 255,
                                         CGFloat(hex & 0xff) / 255, a])!
}
func gradient(_ stops: [(UInt32, CGFloat, CGFloat)]) -> CGGradient {
    CGGradient(colorsSpace: cs, colors: stops.map { rgb($0.0, $0.1) } as CFArray,
               locations: stops.map { $0.2 })!
}

// Seeded RNG so the grain and embers are identical on every render.
var seed: UInt64 = 0x1c0ffee
func rand() -> CGFloat {
    seed = seed &* 6364136223846793005 &+ 1442695040888963407
    return CGFloat(seed >> 33) / CGFloat(1 << 31)
}

// Apple's macOS icon grid: 824 px body, ~185 px continuous corners, centred.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let squircle = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)

// Drop shadow under the tile.
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: 14), blur: 40, color: rgb(0x000000, 0.55))
ctx.addPath(squircle); ctx.setFillColor(rgb(0x0b0c10)); ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(squircle); ctx.clip()

// Plate: gunmetal, lit from the top left.
ctx.drawLinearGradient(gradient([(0x3a3f4b, 1, 0), (0x1b1e25, 1, 0.45), (0x0a0b0f, 1, 1)]),
                       start: CGPoint(x: 180, y: 100), end: CGPoint(x: 820, y: 924), options: [])
ctx.drawRadialGradient(gradient([(0xff5a1f, 0.32, 0), (0xff5a1f, 0, 1)]),
                       startCenter: CGPoint(x: 540, y: 540), startRadius: 0,
                       endCenter: CGPoint(x: 540, y: 540), endRadius: 470, options: [])

// Brushed-steel grain: faint diagonal hairlines.
for _ in 0..<900 {
    let y = rand() * 1400 - 200, len = 80 + rand() * 260, x = rand() * 1200 - 100
    ctx.setStrokeColor(rand() > 0.5 ? rgb(0xffffff, 0.035 * rand()) : rgb(0x000000, 0.08 * rand()))
    ctx.setLineWidth(0.6 + rand() * 1.2)
    ctx.move(to: CGPoint(x: x, y: y)); ctx.addLine(to: CGPoint(x: x + len, y: y - len * 0.35))
    ctx.strokePath()
}

// One slash: a crescent tapering to points at both ends, bowed to the left.
func slash(_ a: CGPoint, _ b: CGPoint, width w: CGFloat, bow: CGFloat) -> CGPath {
    let mid = CGPoint(x: (a.x + b.x) / 2, y: (a.y + b.y) / 2)
    let dx = b.x - a.x, dy = b.y - a.y, len = hypot(dx, dy)
    let n = CGPoint(x: -dy / len, y: dx / len)                 // normal pointing to the left of a→b
    let outer = CGPoint(x: mid.x + n.x * (bow + w), y: mid.y + n.y * (bow + w))
    let inner = CGPoint(x: mid.x + n.x * (bow - w), y: mid.y + n.y * (bow - w))
    let p = CGMutablePath()
    p.move(to: a)
    p.addQuadCurve(to: b, control: outer)
    p.addQuadCurve(to: a, control: inner)
    p.closeSubpath()
    return p
}

let slashes: [(CGPoint, CGPoint, CGFloat)] = [
    (CGPoint(x: 400, y: 225), CGPoint(x: 250, y: 775), 62),
    (CGPoint(x: 600, y: 190), CGPoint(x: 435, y: 845), 74),
    (CGPoint(x: 790, y: 240), CGPoint(x: 625, y: 790), 62),
]

for (a, b, w) in slashes {
    let shape = slash(a, b, width: w, bow: -70)

    // Heat bloom spilling onto the plate.
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: 120, color: rgb(0xff4d00, 1))
    ctx.addPath(shape); ctx.setFillColor(rgb(0xff4d00)); ctx.fillPath()
    ctx.setShadow(offset: .zero, blur: 30, color: rgb(0xff8a1f, 1))
    ctx.addPath(shape); ctx.fillPath()
    ctx.restoreGState()

    // Molten fill: deep red rims, orange body, white-hot core.
    ctx.saveGState()
    ctx.addPath(shape); ctx.clip()
    ctx.drawLinearGradient(gradient([(0xb3140a, 1, 0), (0xff5a12, 1, 0.5), (0xffd27a, 1, 1)]),
                           start: b, end: a, options: [])
    ctx.restoreGState()

    ctx.saveGState()
    let core = slash(CGPoint(x: a.x + (b.x - a.x) * 0.08, y: a.y + (b.y - a.y) * 0.08),
                     CGPoint(x: a.x + (b.x - a.x) * 0.86, y: a.y + (b.y - a.y) * 0.86),
                     width: w * 0.42, bow: -76)
    ctx.setShadow(offset: .zero, blur: 18, color: rgb(0xfff1c2, 1))
    ctx.addPath(core); ctx.setFillColor(rgb(0xfffaf0)); ctx.fillPath()
    ctx.restoreGState()

    // Torn steel lip: dark inner shadow on the upper-left edge, bright rim catching the light.
    ctx.saveGState()
    ctx.addPath(shape); ctx.clip()
    ctx.translateBy(x: 4, y: 6)
    ctx.addRect(CGRect(x: -100, y: -100, width: 1300, height: 1300)); ctx.addPath(shape)
    ctx.setShadow(offset: .zero, blur: 10, color: rgb(0x000000, 0.9))
    ctx.setFillColor(rgb(0x000000, 0.75)); ctx.fillPath(using: .evenOdd)
    ctx.restoreGState()

    ctx.saveGState()
    ctx.addPath(shape); ctx.setLineWidth(3); ctx.setStrokeColor(rgb(0xffe0b0, 0.55)); ctx.strokePath()
    ctx.restoreGState()
}

// Embers drifting up out of the cuts.
for _ in 0..<26 {
    let x = 260 + rand() * 520, y = 170 + rand() * 640, r = 1.5 + rand() * 4.5
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: r * 4, color: rgb(0xff7a1a, 1))
    ctx.setFillColor(rgb(rand() > 0.6 ? 0xfff0c0 : 0xffa040, 0.55 + rand() * 0.45))
    ctx.fillEllipse(in: CGRect(x: x, y: y, width: r, height: r))
    ctx.restoreGState()
}

// Glass rim: top highlight and a soft vignette so the tile reads as a solid object.
ctx.drawRadialGradient(gradient([(0x000000, 0, 0.55), (0x000000, 0.55, 1)]),
                       startCenter: CGPoint(x: 512, y: 470), startRadius: 0,
                       endCenter: CGPoint(x: 512, y: 470), endRadius: 640, options: [])
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(squircle); ctx.clip()
ctx.addPath(CGPath(roundedRect: body.insetBy(dx: 1.5, dy: 1.5), cornerWidth: 184, cornerHeight: 184, transform: nil))
ctx.replacePathWithStrokedPath(); ctx.clip()
ctx.drawLinearGradient(gradient([(0xffffff, 0.35, 0), (0xffffff, 0.04, 0.5), (0xffffff, 0.12, 1)]),
                       start: CGPoint(x: 0, y: 100), end: CGPoint(x: 0, y: 924), options: [])
ctx.restoreGState()

let out = URL(fileURLWithPath: CommandLine.arguments[1])
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
CGImageDestinationFinalize(dest)
