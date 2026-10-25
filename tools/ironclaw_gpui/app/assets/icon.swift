// Renders the IronClaw app icon (1024 px PNG) with CoreGraphics.
// Usage: swift icon.swift <out.png> [core|orbit|eclipse|pulse|aperture|chrome|eye|visor]
// Concept: monochrome AI mark on an obsidian tile. "visor" = a single glowing bot visor slit,
// "core" = a white ring split by a diagonal cut around a solid core.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size = 1024.0
let variant = CommandLine.arguments.count > 2 ? CommandLine.arguments[2] : "crescent"
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

// Apple's macOS icon grid: 824 px body, ~185 px continuous corners, centred.
let body = CGRect(x: 100, y: 100, width: 824, height: 824)
let squircle = CGPath(roundedRect: body, cornerWidth: 185, cornerHeight: 185, transform: nil)
let c = CGPoint(x: 512, y: 512)

// Drop shadow under the tile.
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: 14), blur: 40, color: rgb(0x000000, 0.6))
ctx.addPath(squircle); ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
ctx.restoreGState()

ctx.saveGState()
ctx.addPath(squircle); ctx.clip()

// Obsidian: near-black with a faint cool lift at the top.
ctx.drawLinearGradient(gradient([(0x1c1d22, 1, 0), (0x0a0a0c, 1, 0.55), (0x030304, 1, 1)]),
                       start: CGPoint(x: 512, y: 100), end: CGPoint(x: 512, y: 924), options: [])

// White mark with a halo so it glows against the black.
func glowFill(_ path: CGPath) {
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: 90, color: rgb(0xcfd8ff, 0.55))
    ctx.addPath(path); ctx.setFillColor(rgb(0xffffff)); ctx.fillPath()
    ctx.setShadow(offset: .zero, blur: 22, color: rgb(0xffffff, 0.8))
    ctx.addPath(path); ctx.fillPath()
    ctx.restoreGState()
}

func pol(_ r: CGFloat, _ a: CGFloat) -> CGPoint { CGPoint(x: c.x + r * cos(a), y: c.y + r * sin(a)) }

if variant == "eye" {
    // Crimson iris with three curved black blades spinning out of the pupil (sharingan-inspired).
    let R = 300.0
    let iris = CGPath(ellipseIn: CGRect(x: c.x - R, y: c.y - R, width: 2 * R, height: 2 * R), transform: nil)
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: 120, color: rgb(0xff1a2e, 0.55))
    ctx.addPath(iris); ctx.setFillColor(rgb(0xb0081a)); ctx.fillPath()
    ctx.restoreGState()
    ctx.saveGState()
    ctx.addPath(iris); ctx.clip()
    ctx.drawRadialGradient(gradient([(0xff4a3d, 1, 0), (0xe0142a, 1, 0.45), (0x8a0614, 1, 0.85), (0x3a0208, 1, 1)]),
                           startCenter: c, startRadius: 0, endCenter: c, endRadius: R, options: [])
    // Faint radial fibres in the iris.
    ctx.setStrokeColor(rgb(0x000000, 0.10)); ctx.setLineWidth(3)
    for i in 0..<72 {
        let a = CGFloat(i) / 72 * 2 * .pi
        ctx.move(to: pol(90, a)); ctx.addLine(to: pol(R, a + 0.05))
    }
    ctx.strokePath()
    // Light catch on the upper left.
    ctx.drawRadialGradient(gradient([(0xffffff, 0.22, 0), (0xffffff, 0, 1)]),
                           startCenter: pol(170, -2.3), startRadius: 0, endCenter: pol(170, -2.3), endRadius: 120, options: [])
    ctx.restoreGState()

    // Three blades: each sweeps from the pupil outward and curls clockwise to a point near the rim.
    let blades = CGMutablePath()
    for k in 0..<3 {
        let a = -.pi / 2 + CGFloat(k) * 2 * .pi / 3
        blades.move(to: pol(70, a - 0.55))
        blades.addQuadCurve(to: pol(286, a + 1.35), control: pol(285, a + 0.1))
        blades.addQuadCurve(to: pol(70, a + 0.45), control: pol(215, a + 0.95))
        blades.closeSubpath()
    }
    blades.addEllipse(in: CGRect(x: c.x - 92, y: c.y - 92, width: 184, height: 184))
    ctx.saveGState()
    ctx.setShadow(offset: .zero, blur: 18, color: rgb(0x000000, 0.7))
    ctx.addPath(blades); ctx.setFillColor(rgb(0x070707)); ctx.fillPath()
    // Inner ring tying the blades together.
    ctx.addEllipse(in: CGRect(x: c.x - 196, y: c.y - 196, width: 392, height: 392))
    ctx.setLineWidth(14); ctx.setStrokeColor(rgb(0x070707)); ctx.strokePath()
    ctx.restoreGState()
    // Thin red ring inside the pupil, the one bright detail at the centre.
    ctx.addEllipse(in: CGRect(x: c.x - 40, y: c.y - 40, width: 80, height: 80))
    ctx.setLineWidth(8); ctx.setStrokeColor(rgb(0xff2a3a, 0.9)); ctx.strokePath()

    // Dark limbal ring around the iris.
    ctx.addPath(iris); ctx.setLineWidth(18); ctx.setStrokeColor(rgb(0x050506)); ctx.strokePath()
} else if variant != "visor" {
    // Ring family: a ring with straight cuts through it around a core.
    // `paint` fills whatever is clipped (ring area), so each variant picks its own material.
    func ring(_ outer: CGFloat, _ inner: CGFloat, cuts: [CGFloat], gap: CGFloat, paint: () -> Void) {
        let ring = CGMutablePath()
        ring.addEllipse(in: CGRect(x: c.x - outer, y: c.y - outer, width: 2 * outer, height: 2 * outer))
        ring.addEllipse(in: CGRect(x: c.x - inner, y: c.y - inner, width: 2 * inner, height: 2 * inner))
        ctx.saveGState()
        if !cuts.isEmpty {
            let blades = CGMutablePath()
            blades.addRect(CGRect(x: 0, y: 0, width: size, height: size))
            for a in cuts {
                var t = CGAffineTransform(translationX: c.x, y: c.y).rotated(by: a)
                // Full diameter cuts when the cut list is a single angle, radial spokes otherwise.
                let w = cuts.count == 1 ? CGRect(x: -500, y: -gap / 2, width: 1000, height: gap)
                                        : CGRect(x: 0, y: -gap / 2, width: 500, height: gap)
                blades.addPath(CGPath(rect: w, transform: &t))
            }
            ctx.addPath(blades); ctx.clip(using: .evenOdd)
        }
        ctx.addPath(ring); ctx.clip(using: .evenOdd)
        paint()
        ctx.restoreGState()
    }
    func white() { ctx.setFillColor(rgb(0xffffff)); ctx.fill(CGRect(x: 0, y: 0, width: size, height: size)) }
    func disc(_ r: CGFloat) -> CGPath { CGPath(ellipseIn: CGRect(x: c.x - r, y: c.y - r, width: 2 * r, height: 2 * r), transform: nil) }

    switch variant {
    case "orbit":
        // Thin ring, a moon riding on it, core in the middle.
        ring(262, 238, cuts: [-.pi / 4], gap: 70, paint: white)
        glowFill(disc(118))
        glowFill(disc(44).copy(using: [CGAffineTransform(translationX: 250 * cos(-.pi / 4), y: 250 * sin(-.pi / 4))])!)
    case "eclipse":
        // Black core in front of a white corona: an eclipse.
        ctx.saveGState()
        ctx.setShadow(offset: .zero, blur: 140, color: rgb(0xdfe6ff, 0.9))
        ctx.addPath(disc(250)); ctx.setFillColor(rgb(0xffffff)); ctx.fillPath()
        ctx.restoreGState()
        ctx.addPath(disc(232)); ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
        ring(254, 232, cuts: [-.pi / 4], gap: 40, paint: white)
    case "diamond", "crescent", "corona", "blood", "horizon":
        // Eclipse family: black moon, bright rim, coloured corona glow.
        // Optional 3rd arg picks the corona colour: (glow, rim).
        let palettes: [String: (UInt32, UInt32)] = [
            "ice": (0xdfe6ff, 0xffffff), "blood": (0xff3a1a, 0xffc89a), "gold": (0xffa21a, 0xfff0c8),
            "violet": (0x8a4dff, 0xeadcff), "cyan": (0x00c8ff, 0xd8f8ff), "toxic": (0x5dff3a, 0xe6ffd8),
            "rose": (0xff3d8b, 0xffd6e6),
        ]
        let key = CommandLine.arguments.count > 3 ? CommandLine.arguments[3] : (variant == "blood" ? "blood" : "ice")
        let (glowHex, rim) = palettes[key] ?? palettes["ice"]!
        let glow = rgb(glowHex, 0.95)
        func corona() {
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: 140, color: glow)
            ctx.addPath(disc(250)); ctx.setFillColor(rgb(rim)); ctx.fillPath()
            ctx.restoreGState()
        }
        func fill(_ hex: UInt32) -> () -> Void {
            { ctx.setFillColor(rgb(hex)); ctx.fill(CGRect(x: 0, y: 0, width: size, height: size)) }
        }
        // Accretion band of the "horizon" black hole: a tilted flat ellipse.
        let tilt = CGAffineTransform(translationX: c.x, y: c.y).rotated(by: -0.18)
        func band(_ alpha: CGFloat) {
            var t = tilt
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: 50, color: rgb(0xffd9a8, 0.9))
            ctx.addPath(CGPath(ellipseIn: CGRect(x: -380, y: -70, width: 760, height: 140), transform: &t))
            ctx.setLineWidth(26); ctx.setStrokeColor(rgb(0xfff1dc, alpha)); ctx.strokePath()
            ctx.restoreGState()
        }

        if variant == "corona" {
            // Streamers: thin rays of uneven length fanning out of the rim.
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: 30, color: rgb(0xdfe6ff, 0.8))
            ctx.setFillColor(rgb(0xffffff, 0.35))
            for i in 0..<28 {
                let a = CGFloat(i) / 28 * 2 * .pi
                let len = 290 + 90 * abs(sin(CGFloat(i) * 2.7)) + 40 * abs(cos(CGFloat(i) * 1.3))
                ctx.move(to: pol(240, a - 0.07)); ctx.addLine(to: pol(len, a)); ctx.addLine(to: pol(240, a + 0.07))
                ctx.closePath()
            }
            ctx.fillPath()
            ctx.restoreGState()
        }
        if variant == "horizon" { band(0.9) }
        corona()
        if variant == "crescent" {
            // Moon slid toward the bottom left: only a crescent of light on the top right.
            ctx.addPath(disc(240).copy(using: [CGAffineTransform(translationX: -26, y: 26)])!)
            ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
        } else {
            ctx.addPath(disc(232)); ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
            ring(254, 232, cuts: variant == "horizon" ? [] : [-.pi / 4], gap: 40, paint: fill(rim))
        }
        if variant == "horizon" {
            // Front half of the band crosses over the black hole.
            ctx.saveGState()
            var t = tilt
            ctx.addPath(CGPath(rect: CGRect(x: -600, y: 0, width: 1200, height: 600), transform: &t)); ctx.clip()
            band(1)
            ctx.restoreGState()
        }
        if variant == "diamond" {
            // Diamond-ring flare: one blinding bead on the rim with a four-point star.
            let p = pol(243, -2.25)
            ctx.drawRadialGradient(gradient([(0xffffff, 1, 0), (0xe8eeff, 0.6, 0.25), (0xdfe6ff, 0, 1)]),
                                   startCenter: p, startRadius: 0, endCenter: p, endRadius: 150, options: [])
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: 20, color: rgb(0xffffff, 1))
            ctx.setFillColor(rgb(0xffffff))
            for (a, l) in [(0.0, 230.0), (.pi / 2, 230.0), (.pi / 4, 90.0), (3 * .pi / 4, 90.0)] {
                for s in [1.0, -1.0] {
                    let tip = CGPoint(x: p.x + s * l * cos(a), y: p.y + s * l * sin(a))
                    let w = l > 100 ? 9.0 : 5.0
                    ctx.move(to: CGPoint(x: p.x - w * sin(a), y: p.y + w * cos(a)))
                    ctx.addLine(to: tip)
                    ctx.addLine(to: CGPoint(x: p.x + w * sin(a), y: p.y - w * cos(a)))
                    ctx.closePath()
                }
            }
            ctx.fillPath()
            ctx.addPath(disc(22).copy(using: [CGAffineTransform(translationX: p.x - c.x, y: p.y - c.y)])!)
            ctx.fillPath()
            ctx.restoreGState()
        }
    case "pulse":
        // Core sending out rings that fade with distance, all cut by the same blade.
        for (i, r) in [(0, 210.0), (1, 290.0), (2, 362.0)] {
            ring(r, r - 34 + CGFloat(i) * 8, cuts: [-.pi / 4], gap: 60) {
                ctx.setFillColor(rgb(0xffffff, [1, 0.55, 0.22][i])); ctx.fill(CGRect(x: 0, y: 0, width: size, height: size))
            }
        }
        glowFill(disc(100))
    case "aperture":
        // Three spoke cuts split the ring into a camera-shutter triad.
        ring(285, 195, cuts: [-.pi / 2, .pi / 6, 5 * .pi / 6], gap: 46, paint: white)
        glowFill(disc(96))
    case "chrome":
        // Polished steel ring with a hot cyan core.
        ctx.saveGState()
        ctx.setShadow(offset: .zero, blur: 50, color: rgb(0x000000, 0.9))
        ring(278, 196, cuts: [-.pi / 4], gap: 64) {
            ctx.drawLinearGradient(gradient([(0xffffff, 1, 0), (0x8a8f9a, 1, 0.35), (0xf2f4f8, 1, 0.5),
                                             (0x4a4e57, 1, 0.75), (0xc9ced8, 1, 1)]),
                                   start: CGPoint(x: 260, y: 230), end: CGPoint(x: 760, y: 800), options: [])
        }
        ctx.restoreGState()
        ctx.saveGState()
        ctx.setShadow(offset: .zero, blur: 110, color: rgb(0x00d8ff, 0.9))
        ctx.addPath(disc(92)); ctx.setFillColor(rgb(0x7ff0ff)); ctx.fillPath()
        ctx.setShadow(offset: .zero, blur: 24, color: rgb(0xffffff, 0.9))
        ctx.addPath(disc(56)); ctx.setFillColor(rgb(0xffffff)); ctx.fillPath()
        ctx.restoreGState()
    default:
        // "core": ring cut by a diagonal blade, solid core at the centre.
        ring(270, 200, cuts: [-.pi / 4], gap: 68, paint: white)
        glowFill(disc(92))
    }
} else {
    // Helmet: a faint outline arc framing the visor.
    ctx.saveGState()
    let helmet = CGPath(roundedRect: CGRect(x: 230, y: 250, width: 564, height: 520),
                        cornerWidth: 230, cornerHeight: 230, transform: nil)
    ctx.addPath(helmet)
    ctx.setLineWidth(26); ctx.setStrokeColor(rgb(0xffffff, 0.14)); ctx.strokePath()
    ctx.restoreGState()

    // Visor slit: white capsule with a cool glow, slightly above centre.
    let visor = CGPath(roundedRect: CGRect(x: 300, y: 440, width: 424, height: 104),
                       cornerWidth: 52, cornerHeight: 52, transform: nil)
    glowFill(visor)

    // Scan line: a brighter thin band across the visor.
    ctx.saveGState()
    ctx.addPath(visor); ctx.clip()
    ctx.drawLinearGradient(gradient([(0xdfe6ff, 1, 0), (0xffffff, 1, 0.5), (0xdfe6ff, 1, 1)]),
                           start: CGPoint(x: 300, y: 0), end: CGPoint(x: 724, y: 0), options: [])
    ctx.restoreGState()
}

// Vignette so the tile reads as a solid object.
ctx.drawRadialGradient(gradient([(0x000000, 0, 0.6), (0x000000, 0.5, 1)]),
                       startCenter: CGPoint(x: 512, y: 470), startRadius: 0,
                       endCenter: CGPoint(x: 512, y: 470), endRadius: 660, options: [])
ctx.restoreGState()

// Glass rim: thin top highlight.
ctx.saveGState()
ctx.addPath(squircle); ctx.clip()
ctx.addPath(CGPath(roundedRect: body.insetBy(dx: 1.5, dy: 1.5), cornerWidth: 184, cornerHeight: 184, transform: nil))
ctx.replacePathWithStrokedPath(); ctx.clip()
ctx.drawLinearGradient(gradient([(0xffffff, 0.28, 0), (0xffffff, 0.03, 0.5), (0xffffff, 0.08, 1)]),
                       start: CGPoint(x: 0, y: 100), end: CGPoint(x: 0, y: 924), options: [])
ctx.restoreGState()

let out = URL(fileURLWithPath: CommandLine.arguments[1])
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
CGImageDestinationFinalize(dest)
