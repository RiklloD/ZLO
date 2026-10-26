// Renders the IronClaw app icon (1024 px PNG) with CoreGraphics.
// Usage: swift icon.swift <out.png> [core|orbit|eclipse|pulse|aperture|chrome|eye|visor]
// Concept: monochrome AI mark on an obsidian tile. "visor" = a single glowing bot visor slit,
// "core" = a white ring split by a diagonal cut around a solid core.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size = 1024.0
let variant = CommandLine.arguments.count > 2 ? CommandLine.arguments[2] : "pc_eye"
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

let bold = ["frost", "blob", "tube", "rise", "signal", "aurora", "synth", "portal", "saturn", "lunar"] + ["neonring", "neonsat", "nm_clean", "nm_bare", "nm_cyclops", "nm_orb", "nm_black", "nm_thin", "nc_happy", "nc_look", "nc_blink", "nc_scan", "nc_tilt", "nc_peek", "nc_heart", "nc_sleep", "nc_zoom", "mc_noring", "mc_dome", "mc_flat", "mc_line", "mc_black", "mc_pill", "mh_flat", "mh_black", "mh_mono", "mh_level", "mh_line", "mh_soft", "sunsat", "planet", "gridsat", "ns_cyan", "ns_atom", "ns_wire", "ss_dusk", "ss_ice", "ss_moon", "min_flat", "min_mono", "min_glyph", "min_solid", "min_duo", "mf_thick", "mf_solid", "mf_pink", "mf_black", "mf_tilt", "mf_dusk", "mf_color", "mf_steel", "st_visor", "st_spark", "st_chat", "st_wink", "st_twin", "st_peek", "st_orb", "st_smile", "st_tall", "st_giant", "st_shades", "st_chomp", "st_bounce", "st_hang", "st_hello", "st_love", "st_sleep", "st_think", "st_ghost", "st_ufo", "st_cyclops", "st_duo", "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt", "pc_eye", "pc_round", "pc_look", "pc_wide", "pc_happy", "pc_scan"]

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
} else if bold.contains(variant) {
    // Bold bot mascots grown out of the "antenna" face.
    func circ(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat) -> CGPath {
        CGPath(ellipseIn: CGRect(x: x - r, y: y - r, width: 2 * r, height: 2 * r), transform: nil)
    }
    func pill(_ x: CGFloat, _ y: CGFloat, _ w: CGFloat, _ h: CGFloat) -> CGPath {
        CGPath(roundedRect: CGRect(x: x - w / 2, y: y - h / 2, width: w, height: h),
               cornerWidth: min(w, h) / 2, cornerHeight: min(w, h) / 2, transform: nil)
    }
    func paint(_ p: CGPath, _ hex: UInt32, _ a: CGFloat = 1, glow: UInt32? = nil, blur: CGFloat = 0) {
        ctx.saveGState()
        if let g = glow { ctx.setShadow(offset: .zero, blur: blur, color: rgb(g, 0.9)) }
        ctx.addPath(p); ctx.setFillColor(rgb(hex, a)); ctx.fillPath()
        ctx.restoreGState()
    }
    func stroke(_ a: CGPoint, _ b: CGPoint, ctrl: CGPoint? = nil, _ w: CGFloat) -> CGPath {
        let l = CGMutablePath(); l.move(to: a)
        if let k = ctrl { l.addQuadCurve(to: b, control: k) } else { l.addLine(to: b) }
        return l.copy(strokingWithWidth: w, lineCap: .round, lineJoin: .round, miterLimit: 10)
    }
    func P(_ x: CGFloat, _ y: CGFloat) -> CGPoint { CGPoint(x: x, y: y) }
    let ink: UInt32 = 0x050506, ice: UInt32 = 0xdfe6ff
    // Dark orb with the crescent of light on its top right, like the chosen icon.
    func moon(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat) {
        paint(circ(x, y, r + 8), 0xffffff, glow: ice, blur: 110)
        paint(circ(x - r * 0.11, y + r * 0.11, r), ink)
    }
    func eyes(_ pts: [(CGFloat, CGFloat)], _ w: CGFloat, _ h: CGFloat) {
        let e = CGMutablePath(); pts.forEach { e.addPath(pill($0.0, $0.1, w, h)) }
        glowFill(e)
    }
    switch variant {
    case "frost":
        // Inverted: pale ice tile, ink bot, cyan eyes.
        ctx.drawLinearGradient(gradient([(0xf4f7ff, 1, 0), (0xc9d5ff, 1, 0.6), (0x8fa2f0, 1, 1)]),
                               start: P(300, 100), end: P(700, 924), options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
        paint(stroke(P(512, 380), P(592, 226), ctrl: P(505, 270), 26), ink)
        paint(circ(594, 220, 36), 0x9ff0ff, glow: 0x00c8ff, blur: 60)
        paint(circ(512, 590, 240), ink, glow: 0x1a2250, blur: 90)
        let e = CGMutablePath(); e.addPath(pill(440, 580, 54, 112)); e.addPath(pill(584, 580, 54, 112))
        paint(e, 0xbff6ff, glow: 0x00c8ff, blur: 50)
    case "blob":
        // Inverted mascot: glowing white ghost-bot with ink eyes and blush.
        paint(stroke(P(512, 260), P(582, 162), ctrl: P(505, 190), 24), 0xffffff, glow: ice, blur: 40)
        glowFill(circ(584, 156, 34))
        let g = CGMutablePath()
        g.move(to: P(272, 770)); g.addLine(to: P(272, 490))
        g.addArc(center: P(512, 490), radius: 240, startAngle: .pi, endAngle: 0, clockwise: false)
        g.addLine(to: P(752, 770))
        for i in 0..<3 {
            let x0 = 752 - 160 * CGFloat(i)
            g.addQuadCurve(to: P(x0 - 160, 770), control: P(x0 - 80, 850))
        }
        g.closeSubpath()
        glowFill(g)
        let e = CGMutablePath(); e.addPath(pill(440, 530, 58, 120)); e.addPath(pill(584, 530, 58, 120))
        paint(e, ink)
        paint(circ(450, 500, 11), 0xffffff); paint(circ(594, 500, 11), 0xffffff)
        paint(CGPath(ellipseIn: CGRect(x: 350, y: 610, width: 70, height: 36), transform: nil), 0x9fb4ff, 0.8)
        paint(CGPath(ellipseIn: CGRect(x: 604, y: 610, width: 70, height: 36), transform: nil), 0x9fb4ff, 0.8)
    case "tube":
        // Retro CRT bot: rabbit-ear antennas, glowing screen face with scanlines.
        paint(stroke(P(462, 340), P(352, 182), 18), 0x9aa0ad)
        paint(stroke(P(562, 340), P(688, 200), 18), 0x9aa0ad)
        glowFill(circ(352, 182, 28)); glowFill(circ(688, 200, 28))
        paint(pill(360, 790, 100, 44), 0x2a2d34); paint(pill(664, 790, 100, 44), 0x2a2d34)
        let shell = CGPath(roundedRect: CGRect(x: 222, y: 320, width: 580, height: 460),
                           cornerWidth: 120, cornerHeight: 120, transform: nil)
        ctx.saveGState(); ctx.addPath(shell); ctx.clip()
        ctx.drawLinearGradient(gradient([(0x50545f, 1, 0), (0x24262c, 1, 0.5), (0x111216, 1, 1)]),
                               start: P(512, 320), end: P(512, 780), options: [])
        ctx.restoreGState()
        ctx.addPath(shell); ctx.setLineWidth(4); ctx.setStrokeColor(rgb(0xffffff, 0.18)); ctx.strokePath()
        let screen = CGPath(roundedRect: CGRect(x: 272, y: 370, width: 480, height: 330),
                            cornerWidth: 90, cornerHeight: 90, transform: nil)
        paint(screen, 0x0a1428, glow: 0x6f9cff, blur: 70)
        ctx.saveGState(); ctx.addPath(screen); ctx.clip()
        ctx.drawRadialGradient(gradient([(0x2c4f96, 1, 0), (0x0a1428, 1, 1)]),
                               startCenter: P(512, 530), startRadius: 0, endCenter: P(512, 530), endRadius: 300, options: [])
        eyes([(430, 515), (594, 515)], 56, 110)
        glowFill(stroke(P(470, 612), P(554, 612), ctrl: P(512, 650), 18))
        ctx.setFillColor(rgb(0x000000, 0.22))
        for y in stride(from: 370.0, to: 700, by: 12) { ctx.fill(CGRect(x: 272, y: y, width: 480, height: 5)) }
        ctx.drawLinearGradient(gradient([(0xffffff, 0.16, 0), (0xffffff, 0, 1)]),
                               start: P(300, 370), end: P(420, 520), options: [])
        ctx.restoreGState()
    case "rise":
        // Giant bot face rising over the bottom edge like a planet, antenna as the brightest star.
        for (x, y, r) in [(220.0, 230.0, 5.0), (330, 160, 3), (770, 190, 4), (830, 340, 3), (180, 410, 3),
                          (700, 320, 3), (420, 250, 2.5), (860, 470, 2.5)] {
            paint(circ(x, y, r), 0xffffff, glow: ice, blur: 12)
        }
        paint(stroke(P(512, 540), P(612, 300), ctrl: P(505, 390), 30), ink, glow: ice, blur: 30)
        glowFill(circ(614, 292, 44))
        paint(circ(512, 1050, 548), 0xffffff, glow: ice, blur: 140)
        paint(circ(512, 1064, 540), ink)
        eyes([(410, 690), (614, 690)], 76, 160)
    case "aurora", "synth", "portal", "saturn", "lunar", "neonring", "neonsat", "nm_clean", "nm_bare", "nm_cyclops", "nm_orb", "nm_black", "nm_thin", "nc_happy", "nc_look", "nc_blink", "nc_scan", "nc_tilt", "nc_peek", "nc_heart", "nc_sleep", "nc_zoom", "mc_noring", "mc_dome", "mc_flat", "mc_line", "mc_black", "mc_pill", "mh_flat", "mh_black", "mh_mono", "mh_level", "mh_line", "mh_soft", "sunsat", "planet", "gridsat", "ns_cyan", "ns_atom", "ns_wire", "ss_dusk", "ss_ice", "ss_moon", "min_flat", "min_mono", "min_glyph", "min_solid", "min_duo", "mf_thick", "mf_solid", "mf_pink", "mf_black", "mf_tilt", "mf_dusk", "mf_color", "mf_steel", "st_visor", "st_spark", "st_chat", "st_wink", "st_twin", "st_peek", "st_orb", "st_smile", "st_tall", "st_giant", "st_shades", "st_chomp", "st_bounce", "st_hang", "st_hello", "st_love", "st_sleep", "st_think", "st_ghost", "st_ufo", "st_cyclops", "st_duo", "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt", "pc_eye", "pc_round", "pc_look", "pc_wide", "pc_happy", "pc_scan":
        // Rise family: the giant rising bot head in different worlds.
        func head(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat, _ glow: UInt32, rim: UInt32 = 0xffffff) {
            paint(circ(x, y - r * 0.026, r + 8), rim, glow: glow, blur: 140)
            paint(circ(x, y, r), ink)
        }
        func antenna(_ x: CGFloat, _ top: CGFloat, _ k: CGFloat, _ glow: UInt32, tip: UInt32 = 0xffffff) {
            paint(stroke(P(x, top), P(x + 100 * k, top - 240 * k), ctrl: P(x - 7 * k, top - 150 * k), 30 * k),
                  ink, glow: glow, blur: 30)
            paint(circ(x + 102 * k, top - 248 * k, 44 * k), tip, glow: glow, blur: 60)
        }
        func lit(_ pts: [(CGFloat, CGFloat)], _ w: CGFloat, _ h: CGFloat, _ hex: UInt32, _ glow: UInt32) {
            let e = CGMutablePath(); pts.forEach { e.addPath(pill($0.0, $0.1, w, h)) }
            paint(e, hex, glow: glow, blur: 60)
        }
        func stars(_ list: [(CGFloat, CGFloat, CGFloat)], _ glow: UInt32 = ice) {
            for (x, y, r) in list { paint(circ(x, y, r), 0xffffff, glow: glow, blur: 12) }
        }
        let sky: [(CGFloat, CGFloat, CGFloat)] = [(220, 230, 5), (330, 160, 3), (770, 190, 4), (830, 340, 3),
                                                  (180, 410, 3), (700, 320, 3), (420, 250, 2.5), (860, 470, 2.5)]
        // Synth x Saturn mixes: shared sky, grid, striped sun and tilted ring.
        let pink: UInt32 = 0xff2e88, cyan: UInt32 = 0x00c8ff
        func synthSky() {
            ctx.drawLinearGradient(gradient([(0x0b0220, 1, 0), (0x2a0a4a, 1, 0.45), (0x7a1268, 1, 0.66), (0x0b0220, 1, 0.67)]),
                                   start: P(512, 100), end: P(512, 924), options: [])
        }
        func grid(_ horizon: CGFloat) {
            ctx.saveGState()
            ctx.clip(to: CGRect(x: 0, y: horizon, width: size, height: size - horizon))
            ctx.setStrokeColor(rgb(0xff3fc0, 0.85)); ctx.setLineWidth(5)
            ctx.setShadow(offset: .zero, blur: 18, color: rgb(pink, 1))
            for i in -8...8 { ctx.move(to: P(512 + CGFloat(i) * 40, horizon)); ctx.addLine(to: P(512 + CGFloat(i) * 230, size)) }
            var y = horizon + 10, step: CGFloat = 14
            while y < size { ctx.move(to: P(0, y)); ctx.addLine(to: P(size, y)); step *= 1.45; y += step }
            ctx.strokePath()
            ctx.restoreGState()
            paint(CGPath(rect: CGRect(x: 0, y: horizon - 3, width: size, height: 6), transform: nil), 0xffd0f0, glow: pink, blur: 30)
        }
        // Striped sunset disc; stripes are cut in the lower third.
        func sunFill(_ shape: CGPath, _ top: CGFloat, _ bottom: CGFloat, stripes: [CGFloat],
                     colors: [(UInt32, CGFloat, CGFloat)]? = nil, glow: UInt32? = nil) {
            paint(shape, 0xffffff, glow: glow ?? pink, blur: 120)
            ctx.saveGState(); ctx.addPath(shape); ctx.clip()
            ctx.drawLinearGradient(gradient(colors ?? [(0xffe46b, 1, 0), (0xff8a3d, 1, 0.5), (pink, 1, 1)]),
                                   start: P(512, top), end: P(512, bottom), options: [])
            ctx.setBlendMode(.clear)
            for (i, y) in stripes.enumerated() { ctx.fill(CGRect(x: 0, y: y, width: size, height: 8 + 4 * CGFloat(i))) }
            ctx.restoreGState()
        }
        func tilt(_ cx: CGFloat, _ cy: CGFloat, _ a: CGFloat) -> CGAffineTransform {
            CGAffineTransform(translationX: cx, y: cy).rotated(by: a)
        }
        func ringShape(_ cx: CGFloat, _ cy: CGFloat, _ rx: CGFloat, _ ry: CGFloat, _ a: CGFloat) -> CGPath {
            let t = tilt(cx, cy, a), o = CGMutablePath()
            o.addEllipse(in: CGRect(x: -rx, y: -ry, width: 2 * rx, height: 2 * ry), transform: t)
            let i = CGMutablePath()
            i.addEllipse(in: CGRect(x: -rx * 0.89, y: -ry * 0.75, width: 1.78 * rx, height: 1.5 * ry), transform: t)
            return o.copy(strokingWithWidth: 26, lineCap: .round, lineJoin: .round, miterLimit: 10)
                .union(i.copy(strokingWithWidth: 8, lineCap: .round, lineJoin: .round, miterLimit: 10))
        }
        // Back half dim behind the body, front half bright on top of it.
        func ringed(_ cx: CGFloat, _ cy: CGFloat, _ rx: CGFloat, _ ry: CGFloat, _ a: CGFloat,
                    _ hex: UInt32, _ glow: UInt32, body: () -> Void) {
            let r = ringShape(cx, cy, rx, ry, a)
            paint(r, hex, 0.55, glow: glow, blur: 40)
            body()
            ctx.saveGState()
            var t = tilt(cx, cy, a)
            ctx.addPath(CGPath(rect: CGRect(x: -700, y: 0, width: 1400, height: 600), transform: &t)); ctx.clip()
            paint(r, hex, glow: glow, blur: 40)
            ctx.restoreGState()
        }
        switch variant {
        case "neonring":
            // Synth sun on the grid, wearing a neon ring.
            synthSky(); stars(sky.filter { $0.1 < 330 }, 0xff6ad5)
            paint(stroke(P(512, 300), P(600, 160), ctrl: P(505, 220), 26), 0xff6ad5, glow: pink, blur: 30)
            paint(circ(602, 154, 38), 0xffffff, glow: pink, blur: 60)
            ringed(512, 520, 420, 92, -0.14, 0x9ff0ff, cyan) {
                ctx.saveGState(); ctx.clip(to: CGRect(x: 0, y: 0, width: size, height: 650))
                sunFill(circ(512, 540, 240), 300, 650, stripes: [560, 594, 624])
                lit([(430, 480), (594, 480)], 62, 130, 0x1a0630, pink)
                ctx.restoreGState()
            }
            grid(650)
        // Neonsat pared back: no stars, one ring line (or none), soft glows kept.
        case "nm_clean", "nm_bare", "nm_cyclops", "nm_orb", "nm_black", "nm_thin", "nc_happy", "nc_look", "nc_blink", "nc_scan", "nc_tilt", "nc_peek", "nc_heart", "nc_sleep", "nc_zoom", "mc_noring", "mc_dome", "mc_flat", "mc_line", "mc_black", "mc_pill", "mh_flat", "mh_black", "mh_mono", "mh_level", "mh_line", "mh_soft":
            func ring1(_ cx: CGFloat, _ cy: CGFloat, _ rx: CGFloat, _ ry: CGFloat, _ a: CGFloat, _ lw: CGFloat,
                       body: () -> Void) {
                let o = CGMutablePath()
                o.addEllipse(in: CGRect(x: -rx, y: -ry, width: 2 * rx, height: 2 * ry), transform: tilt(cx, cy, a))
                let r = o.copy(strokingWithWidth: lw, lineCap: .round, lineJoin: .round, miterLimit: 10)
                paint(r, 0xff8ad8, 0.5, glow: pink, blur: 30)
                body()
                ctx.saveGState()
                var t = tilt(cx, cy, a)
                ctx.addPath(CGPath(rect: CGRect(x: -700, y: 0, width: 1400, height: 600), transform: &t)); ctx.clip()
                paint(r, 0xff8ad8, glow: pink, blur: 30)
                ctx.restoreGState()
            }
            switch variant {
            case "nm_clean":
                // Neonsat minus stars and the inner ring.
                synthSky()
                ring1(512, 520, 470, 88, -0.12, 24) {
                    antenna(512, 540, 1, pink, tip: 0xffd0f0)
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                }
                lit([(410, 720), (614, 720)], 76, 150, 0xbff6ff, cyan)
            case "nm_bare":
                // No ring at all: just the glowing rim, antenna and eyes.
                synthSky()
                antenna(512, 540, 1, pink, tip: 0xffd0f0)
                head(512, 1064, 540, pink, rim: 0xffd0f0)
                lit([(410, 700), (614, 700)], 80, 160, 0xbff6ff, cyan)
            case "nm_cyclops":
                // Neon cyclops: one big cyan eye.
                synthSky()
                ring1(512, 520, 470, 88, -0.12, 24) {
                    antenna(512, 540, 1, pink, tip: 0xffd0f0)
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                }
                lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
            case "nm_orb":
                // The whole head as a neon planet floating in the synth sky.
                ctx.drawLinearGradient(gradient([(0x0b0220, 1, 0), (0x2a0a4a, 1, 0.55), (0x4a0e5a, 1, 1)]),
                                       start: P(512, 100), end: P(512, 924), options: [])
                ring1(512, 560, 400, 80, -0.2, 22) {
                    paint(stroke(P(512, 340), P(580, 230), ctrl: P(505, 280), 22), ink, glow: pink, blur: 30)
                    paint(circ(584, 224, 34), 0xffd0f0, glow: pink, blur: 50)
                    head(512, 560, 230, pink, rim: 0xffd0f0)
                }
                lit([(452, 540), (572, 540)], 60, 120, 0xbff6ff, cyan)
            case "nm_black":
                // Same as clean on a plain black tile.
                paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), 0x000000)
                ring1(512, 520, 470, 88, -0.12, 24) {
                    antenna(512, 540, 1, pink, tip: 0xffd0f0)
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                }
                lit([(410, 720), (614, 720)], 76, 150, 0xbff6ff, cyan)
            // Neon cyclops poses: same sky, rim and ring, the single eye changes.
            case "nc_happy", "nc_look", "nc_blink", "nc_scan", "nc_tilt", "nc_heart", "nc_sleep":
                func eyeLine(_ a: CGPoint, _ b: CGPoint, _ k: CGPoint, _ w: CGFloat) {
                    paint(stroke(a, b, ctrl: k, w), 0xbff6ff, glow: cyan, blur: 60)
                }
                synthSky()
                if variant == "nc_tilt" {
                    ctx.saveGState()
                    ctx.translateBy(x: 512, y: 1064); ctx.rotate(by: -0.2); ctx.translateBy(x: -512, y: -1064)
                }
                ring1(512, 520, 470, 88, -0.12, 24) {
                    if variant == "nc_sleep" {
                        // Antenna droops to the side while it naps.
                        paint(stroke(P(512, 540), P(372, 420), ctrl: P(470, 380), 30), ink, glow: pink, blur: 30)
                        paint(circ(366, 426, 40), 0xffd0f0, 0.6, glow: pink, blur: 40)
                    } else {
                        antenna(512, 540, 1, pink, tip: 0xffd0f0)
                    }
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                }
                switch variant {
                case "nc_happy": eyeLine(P(420, 780), P(604, 780), P(512, 640), 46)
                case "nc_look":
                    lit([(586, 680)], 140, 190, 0xbff6ff, cyan)
                    paint(circ(612, 640, 22), 0xffffff)
                case "nc_blink": lit([(512, 740)], 200, 36, 0xbff6ff, cyan)
                case "nc_scan":
                    // Wide dim slot with a bright sweep, like a scanner mid-pass.
                    paint(pill(512, 730, 400, 96), cyan, 0.18)
                    paint(pill(400, 730, 110, 60), cyan, 0.25)
                    lit([(600, 730)], 120, 96, 0xbff6ff, cyan)
                case "nc_heart":
                    let h = CGMutablePath(), k: CGFloat = 105
                    h.addPath(circ(512 - k * 0.5, 700, k * 0.56)); h.addPath(circ(512 + k * 0.5, 700, k * 0.56))
                    h.move(to: P(512 - k * 1.03, 720)); h.addLine(to: P(512 + k * 1.03, 720)); h.addLine(to: P(512, 720 + k * 1.05))
                    h.closeSubpath()
                    paint(h, 0xffd0f0, glow: pink, blur: 60)
                case "nc_sleep": eyeLine(P(430, 740), P(594, 740), P(512, 820), 40)
                default: lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                }
                if variant == "nc_tilt" { ctx.restoreGState() }
            case "nc_peek":
                // Sinking below the horizon: only the dome and the eye peek out.
                synthSky()
                antenna(512, 640, 0.9, pink, tip: 0xffd0f0)
                head(512, 1180, 540, pink, rim: 0xffd0f0)
                lit([(512, 810)], 150, 150, 0xbff6ff, cyan)
            // Neon cyclops stripped down: fewer parts, less glow, flatter sky.
            case "mc_noring", "mc_dome", "mc_flat", "mc_line", "mc_black", "mc_pill":
                func flatSky(_ hex: UInt32) {
                    paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), hex)
                }
                func flatBot() {
                    paint(stroke(P(512, 540), P(612, 300), ctrl: P(505, 390), 26), pink)
                    paint(circ(614, 292, 40), 0xffd0f0)
                    paint(circ(512, 1064, 552), pink)
                    paint(circ(512, 1064, 540), ink)
                }
                switch variant {
                case "mc_noring":
                    // Cyclops without the ring.
                    synthSky()
                    antenna(512, 540, 1, pink, tip: 0xffd0f0)
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                    lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                case "mc_dome":
                    // No ring, no antenna: just the dome and the eye.
                    synthSky()
                    head(512, 1000, 540, pink, rim: 0xffd0f0)
                    lit([(512, 690)], 160, 220, 0xbff6ff, cyan)
                case "mc_flat":
                    // One flat purple, solid pink rim, glow only on the eye.
                    flatSky(0x1e0838)
                    flatBot()
                    lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                case "mc_line":
                    // Flat, with the ring kept as one thin line.
                    flatSky(0x1e0838)
                    let o = CGMutablePath()
                    o.addEllipse(in: CGRect(x: -470, y: -88, width: 940, height: 176), transform: tilt(512, 520, -0.12))
                    let r = o.copy(strokingWithWidth: 12, lineCap: .round, lineJoin: .round, miterLimit: 10)
                    paint(r, pink, 0.45)
                    flatBot()
                    ctx.saveGState()
                    var t = tilt(512, 520, -0.12)
                    ctx.addPath(CGPath(rect: CGRect(x: -700, y: 0, width: 1400, height: 600), transform: &t)); ctx.clip()
                    paint(r, pink)
                    ctx.restoreGState()
                    lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                case "mc_black":
                    // Black tile, pink rim, cyan eye. Nothing else.
                    flatSky(0x000000)
                    head(512, 1000, 540, pink, rim: 0xffd0f0)
                    lit([(512, 690)], 160, 220, 0xbff6ff, cyan)
                default:
                    // mc_pill: no ring, wide visor eye, short antenna stub.
                    synthSky()
                    paint(stroke(P(512, 462), P(512, 530), 24), ink, glow: pink, blur: 20)
                    paint(circ(512, 430, 36), 0xffd0f0, glow: pink, blur: 50)
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                    lit([(512, 730)], 240, 120, 0xbff6ff, cyan)
                }
            // Cyclops with its halo, pared down in shape: flat fills, no glow except the eye.
            case "mh_flat", "mh_black", "mh_mono", "mh_level", "mh_line", "mh_soft":
                func halo(_ cy: CGFloat, _ rx: CGFloat, _ ry: CGFloat, _ a: CGFloat, _ lw: CGFloat,
                          _ hex: UInt32, back: CGFloat = 0.4, gap: UInt32 = 0x050506, body: () -> Void) {
                    let o = CGMutablePath()
                    o.addEllipse(in: CGRect(x: -rx, y: -ry, width: 2 * rx, height: 2 * ry), transform: tilt(512, cy, a))
                    let r = o.copy(strokingWithWidth: lw, lineCap: .round, lineJoin: .round, miterLimit: 10)
                    paint(r, hex, back)
                    body()
                    ctx.saveGState()
                    var t = tilt(512, cy, a)
                    ctx.addPath(CGPath(rect: CGRect(x: -700, y: 0, width: 1400, height: 600), transform: &t)); ctx.clip()
                    // Dark gap so the front of the halo cuts cleanly over the rim.
                    ctx.saveGState(); ctx.addPath(circ(512, 1064, 566)); ctx.clip()
                    paint(o.copy(strokingWithWidth: lw + 36, lineCap: .round, lineJoin: .round, miterLimit: 10), gap)
                    ctx.restoreGState()
                    paint(r, hex)
                    ctx.restoreGState()
                }
                func bot(_ rim: UInt32, _ tip: UInt32, _ y: CGFloat = 1064, rimW: CGFloat = 22, straight: Bool = false) {
                    if straight {
                        paint(stroke(P(512, y - 524), P(512, y - 760), 26), rim)
                        paint(circ(512, y - 780, 40), tip)
                    } else {
                        paint(stroke(P(512, y - 524), P(612, y - 764), ctrl: P(505, y - 674), 26), rim)
                        paint(circ(614, y - 772, 40), tip)
                    }
                    paint(circ(512, y, 540 + rimW), rim)
                    paint(circ(512, y, 540), ink)
                }
                switch variant {
                case "mh_flat":
                    // Flat purple tile, flat pink halo and rim, the eye is the only glow.
                    paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), 0x1e0838)
                    halo(520, 470, 88, -0.12, 26, pink) { bot(pink, 0xffd0f0) }
                    lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                case "mh_black":
                    // Same on black.
                    paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), 0x000000)
                    halo(520, 470, 88, -0.12, 26, pink) { bot(pink, 0xffd0f0) }
                    lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                case "mh_mono":
                    // One colour: pink halo, pink rim, pink eye on deep purple.
                    paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), 0x16052a)
                    halo(520, 470, 88, -0.12, 26, pink, back: 0.35) { bot(pink, pink) }
                    paint(pill(512, 730, 150, 210), pink)
                case "mh_level":
                    // Geometric: level halo, straight antenna, round eye.
                    paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), 0x1e0838)
                    halo(560, 460, 80, 0, 26, pink) { bot(pink, 0xffd0f0, straight: true) }
                    paint(circ(512, 740, 92), 0xbff6ff, glow: cyan, blur: 50)
                case "mh_line":
                    // Thin even strokes everywhere: halo, rim and antenna share one line weight.
                    paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), 0x1e0838)
                    halo(520, 470, 88, -0.12, 14, pink, gap: 0x1e0838) {
                        paint(stroke(P(512, 540), P(612, 300), ctrl: P(505, 390), 14), pink)
                        paint(circ(614, 292, 30), pink)
                        paint(circ(512, 1064, 548), pink)
                        paint(circ(512, 1064, 534), 0x1e0838)
                    }
                    paint(pill(512, 730, 130, 190), 0xbff6ff, glow: cyan, blur: 40)
                default:
                    // mh_soft: keeps the synth sky gradient, everything else flat.
                    synthSky()
                    halo(520, 470, 88, -0.12, 26, 0xff8ad8) { bot(0xff8ad8, 0xffd0f0) }
                    lit([(512, 730)], 150, 210, 0xbff6ff, cyan)
                }
            case "nc_zoom":
                // Close-up: the head fills the tile, eye front and centre.
                synthSky()
                ring1(512, 330, 560, 100, -0.12, 28) {
                    head(512, 1000, 800, pink, rim: 0xffd0f0)
                }
                lit([(512, 600)], 230, 320, 0xbff6ff, cyan)
                paint(circ(560, 520, 30), 0xffffff)
            default:
                // "nm_thin": hairline ring, no antenna, the calmest one.
                synthSky()
                ring1(512, 540, 470, 84, -0.12, 12) {
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                }
                lit([(420, 730), (604, 730)], 70, 140, 0xbff6ff, cyan)
            }
        case "neonsat":
            // Saturn layout repainted in synthwave: pink rim, neon ring, cyan eyes.
            synthSky(); stars(sky, 0xff6ad5)
            ringed(512, 520, 470, 88, -0.12, 0xff8ad8, pink) {
                antenna(512, 540, 1, pink, tip: 0xffd0f0)
                head(512, 1064, 540, pink, rim: 0xffd0f0)
            }
            lit([(410, 720), (614, 720)], 76, 150, 0xbff6ff, cyan)
        case "sunsat":
            // Saturn layout, but the giant head is the striped synth sun.
            stars(sky)
            ringed(512, 520, 470, 88, -0.12, 0xffffff, ice) {
                antenna(512, 540, 1, pink, tip: 0xffe46b)
                sunFill(circ(512, 1064, 540), 524, 924, stripes: [820, 856, 890])
            }
            lit([(410, 690), (614, 690)], 76, 150, 0x1a0630, pink)
        case "planet":
            // Whole striped sun planet floating in space with a cyan ring.
            stars(sky + [(150, 700, 3), (870, 760, 2.5), (300, 860, 2)], 0xff6ad5)
            ringed(512, 540, 430, 96, -0.22, 0x9ff0ff, cyan) {
                paint(stroke(P(512, 320), P(596, 190), ctrl: P(505, 240), 24), 0xff6ad5, glow: pink, blur: 30)
                paint(circ(598, 184, 36), 0xffffff, glow: pink, blur: 60)
                sunFill(circ(512, 540, 220), 320, 760, stripes: [620, 660, 700])
                lit([(442, 500), (582, 500)], 56, 116, 0x1a0630, pink)
            }
        case "gridsat":
            // The black Saturn head rising behind the synth grid.
            synthSky(); stars(sky.filter { $0.1 < 330 }, 0xff6ad5)
            ringed(512, 500, 440, 90, -0.14, 0x9ff0ff, cyan) {
                ctx.saveGState(); ctx.clip(to: CGRect(x: 0, y: 0, width: size, height: 660))
                antenna(512, 370, 0.75, pink, tip: 0xffd0f0)
                head(512, 660, 290, pink, rim: 0xffd0f0)
                lit([(450, 540), (574, 540)], 58, 120, 0xbff6ff, cyan)
                ctx.restoreGState()
            }
            grid(660)
        // Minimal neonsat: flat fills, one ring line, no stars, little or no glow.
        case "min_flat", "min_mono", "min_glyph", "min_solid", "min_duo", "mf_thick", "mf_solid", "mf_pink", "mf_black", "mf_tilt", "mf_dusk", "mf_color", "mf_steel", "st_visor", "st_spark", "st_chat", "st_wink", "st_twin", "st_peek", "st_orb", "st_smile", "st_tall", "st_giant", "st_shades", "st_chomp", "st_bounce", "st_hang", "st_hello", "st_love", "st_sleep", "st_think", "st_ghost", "st_ufo", "st_cyclops", "st_duo", "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt", "pc_eye", "pc_round", "pc_look", "pc_wide", "pc_happy", "pc_scan":
            func flatRing(_ cx: CGFloat, _ cy: CGFloat, _ rx: CGFloat, _ ry: CGFloat, _ a: CGFloat,
                          _ w: CGFloat, _ hex: UInt32, gap: UInt32 = 0x000000, gapClip: CGPath? = nil, body: () -> Void) {
                let o = CGMutablePath()
                o.addEllipse(in: CGRect(x: -rx, y: -ry, width: 2 * rx, height: 2 * ry), transform: tilt(cx, cy, a))
                let r = o.copy(strokingWithWidth: w, lineCap: .round, lineJoin: .round, miterLimit: 10)
                paint(r, hex)
                body()
                ctx.saveGState()
                var t = tilt(cx, cy, a)
                ctx.addPath(CGPath(rect: CGRect(x: -700, y: 0, width: 1400, height: 600), transform: &t)); ctx.clip()
                // A background-coloured halo cuts whatever the front half crosses, so lines never tangle.
                ctx.saveGState()
                if let g = gapClip { ctx.addPath(g); ctx.clip() }
                paint(o.copy(strokingWithWidth: w * 2.6, lineCap: .round, lineJoin: .round, miterLimit: 10), gap)
                ctx.restoreGState()
                paint(r, hex)
                ctx.restoreGState()
            }
            func flatBg(_ hex: UInt32) { paint(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil), hex) }
            func outlineHead(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat, _ line: UInt32, _ w: CGFloat, fill: UInt32 = ink) {
                paint(circ(x, y, r + w / 2), line); paint(circ(x, y, r - w / 2), fill)
            }
            func stick(_ x: CGFloat, _ top: CGFloat, _ len: CGFloat, _ hex: UInt32, _ w: CGFloat, tip: UInt32) {
                paint(stroke(P(x, top + 10), P(x, top - len), w), hex)
                paint(circ(x, top - len, w * 1.6), tip)
            }
            // min_flat forks: one parametric drawing, each fork changes a few knobs.
            func minFlat(bg: UInt32, line: UInt32, eye: UInt32, w: CGFloat = 22, tilt a: CGFloat = -0.12,
                         fill: UInt32 = ink, outline: Bool = true, eyeW: CGFloat = 74, eyeH: CGFloat = 150,
                         eyeY: CGFloat = 730, ringY: CGFloat = 560) {
                flatRing(512, ringY, 440, 80, a, w, line, gap: fill, gapClip: circ(512, 1064, 560)) {
                    stick(512, 540, 150, line, w, tip: line)
                    if outline { outlineHead(512, 1064, 540, line, w, fill: fill) } else { paint(circ(512, 1064, 540), fill) }
                }
                let e = CGMutablePath(); e.addPath(pill(512 - 92, eyeY, eyeW, eyeH)); e.addPath(pill(512 + 92, eyeY, eyeW, eyeH))
                paint(e, eye)
            }
            switch variant {
            case "st_visor", "st_spark", "st_chat", "st_wink", "st_twin", "st_peek", "st_orb", "st_smile", "st_tall", "st_giant", "st_shades", "st_chomp", "st_bounce", "st_hang", "st_hello", "st_love", "st_sleep", "st_think", "st_ghost", "st_ufo", "st_cyclops", "st_duo", "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt", "pc_eye", "pc_round", "pc_look", "pc_wide", "pc_happy", "pc_scan":
                // Steel bot without the ring: brushed metal on black, each variant adds one idea.
                // Optional 3rd arg picks the finish: steel | chrome | gunmetal | graphite | solid | inverse.
                let finish = CommandLine.arguments.count > 3 ? CommandLine.arguments[3] : "steel"
                let light = finish == "inverse", solid = finish == "solid"
                let tile: UInt32 = finish == "graphite" ? 0x1b1c20 : finish == "edge" ? 0x17181b : 0x000000
                let stops: [(UInt32, CGFloat, CGFloat)] = finish == "chrome"
                    ? [(0xffffff, 1, 0), (0xc8ccd4, 1, 0.3), (0x4a4e56, 1, 0.48), (0xe6e8ec, 1, 0.6), (0x7d828b, 1, 1)]
                    : finish == "gunmetal" ? [(0xa9aeb7, 1, 0), (0x5d6169, 1, 0.5), (0x2e3036, 1, 1)]
                    : [(0xf2f4f7, 1, 0), (0xb4b9c2, 1, 0.45), (0x6d727b, 1, 1)]
                if light {
                    ctx.drawLinearGradient(gradient([(0xeef0f3, 1, 0), (0xb9bec6, 1, 0.6), (0x8d929b, 1, 1)]),
                                           start: P(512, 100), end: P(512, 924), options: [])
                } else {
                    flatBg(tile)
                    ctx.drawLinearGradient(gradient([(0xffffff, 0.07, 0), (0xffffff, 0, 0.5)]),
                                           start: P(512, 100), end: P(512, 924), options: [])
                }
                func mfill(_ p: CGPath) {
                    if light { paint(p, 0x0a0a0c); return }
                    ctx.saveGState(); ctx.addPath(p); ctx.clip()
                    ctx.drawLinearGradient(gradient(stops), start: P(512, 180), end: P(512, 924),
                                           options: [.drawsBeforeStartLocation, .drawsAfterEndLocation])
                    ctx.restoreGState()
                }
                let w: CGFloat = 34
                func headO(_ x: CGFloat = 512, _ y: CGFloat = 1064, _ r: CGFloat = 540) {
                    mfill(circ(x, y, r + w / 2))
                    if !solid { paint(circ(x, y, r - w / 2), light ? 0xc4c8cf : tile) }
                }
                func whites(_ p: CGPath) {
                    if solid || light { paint(p, 0x000000) }
                    else if finish == "gunmetal" { paint(p, 0xd4d7dd) }
                    else { paint(p, 0xffffff, glow: 0xdfe6ff, blur: finish == "chrome" ? 50 : 40) }
                }
                func twoEyes(_ y: CGFloat = 730, _ ew: CGFloat = 88, _ eh: CGFloat = 170, _ dx: CGFloat = 92) {
                    let e = CGMutablePath(); e.addPath(pill(512 - dx, y, ew, eh)); e.addPath(pill(512 + dx, y, ew, eh))
                    whites(e)
                }
                func stickUp(_ x: CGFloat = 512, _ top: CGFloat = 390) {
                    mfill(stroke(P(x, 550), P(x, top), w).union(circ(x, top, w * 1.6)))
                }
                func sparkle(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat) -> CGPath {
                    let st = CGMutablePath(); st.move(to: P(x, y - r))
                    for (dx, dy) in [(r, 0.0), (0, r), (-r, 0), (0, -r)] as [(CGFloat, CGFloat)] {
                        st.addQuadCurve(to: P(x + dx, y + dy), control: P(x, y))
                    }
                    st.closeSubpath(); return st
                }
                switch variant {
                case "st_visor":
                    // A metal visor band across the face, eyes glowing inside the slot.
                    stickUp(); headO()
                    let band = CGPath(roundedRect: CGRect(x: 290, y: 640, width: 444, height: 180), cornerWidth: 90, cornerHeight: 90, transform: nil)
                    mfill(band)
                    paint(CGPath(roundedRect: CGRect(x: 318, y: 668, width: 388, height: 124), cornerWidth: 62, cornerHeight: 62, transform: nil), 0x000000)
                    if solid || light {
                        let e = CGMutablePath(); e.addPath(pill(428, 730, 60, 92)); e.addPath(pill(596, 730, 60, 92))
                        paint(e, 0xffffff, glow: 0xdfe6ff, blur: 30)
                    } else { twoEyes(730, 60, 92, 84) }
                case "st_spark":
                    // The antenna ends in an AI sparkle instead of a ball.
                    headO()
                    mfill(stroke(P(512, 550), P(596, 330), ctrl: P(505, 420), w))
                    if solid { paint(sparkle(606, 296, 82), 0xffffff, glow: 0xdfe6ff, blur: 40) } else { whites(sparkle(606, 296, 82)) }
                    twoEyes()
                case "st_chat":
                    // A steel speech bubble with three dots pops out of the antenna.
                    stickUp(470, 400); headO()
                    let b = CGMutablePath()
                    b.addRoundedRect(in: CGRect(x: 560, y: 190, width: 280, height: 170), cornerWidth: 70, cornerHeight: 70)
                    b.move(to: P(600, 340)); b.addLine(to: P(540, 410)); b.addLine(to: P(660, 350)); b.closeSubpath()
                    mfill(b)
                    for x in [636.0, 700, 764] as [CGFloat] { paint(circ(x, 275, 20), 0x000000) }
                    twoEyes()
                case "st_wink":
                    // One pill eye, one closed arc: a wink.
                    stickUp(); headO()
                    whites(pill(420, 730, 88, 170))
                    let arc = CGMutablePath()
                    arc.addArc(center: P(604, 760), radius: 56, startAngle: .pi * 1.1, endAngle: .pi * 1.9, clockwise: false)
                    whites(arc.copy(strokingWithWidth: 30, lineCap: .round, lineJoin: .round, miterLimit: 10))
                case "st_twin":
                    // Two antennas splayed like a bug.
                    mfill(stroke(P(440, 560), P(340, 330), w).union(circ(340, 330, w * 1.6)))
                    mfill(stroke(P(584, 560), P(684, 330), w).union(circ(684, 330, w * 1.6)))
                    headO(); twoEyes()
                case "st_orb":
                    // The whole head as a floating ball with a ground shadow.
                    paint(CGPath(ellipseIn: CGRect(x: 332, y: 806, width: 360, height: 40), transform: nil), 0x000000, 0.0)
                    ctx.saveGState(); ctx.setShadow(offset: .zero, blur: 40, color: rgb(0x9aa0ad, 0.35))
                    paint(CGPath(ellipseIn: CGRect(x: 352, y: 820, width: 320, height: 30), transform: nil), 0x2a2c31)
                    ctx.restoreGState()
                    mfill(stroke(P(512, 330), P(560, 210), ctrl: P(505, 260), w * 0.8).union(circ(562, 204, w * 1.3)))
                    headO(512, 540, 230)
                    let e = CGMutablePath(); e.addPath(pill(450, 530, 64, 120)); e.addPath(pill(574, 530, 64, 120))
                    whites(e)
                case "st_smile":
                    // Peek pose with a small smile under the eyes.
                    ctx.saveGState()
                    ctx.translateBy(x: 512, y: 1000); ctx.rotate(by: -0.2); ctx.translateBy(x: -512, y: -1000)
                    mfill(stroke(P(512, 560), P(560, 340), ctrl: P(500, 430), w).union(circ(562, 334, w * 1.6)))
                    headO(512, 1170, 620)
                    let e = CGMutablePath(); e.addPath(pill(450, 700, 80, 130)); e.addPath(pill(634, 700, 80, 130))
                    let m = CGMutablePath(); m.move(to: P(500, 820)); m.addQuadCurve(to: P(584, 820), control: P(542, 862))
                    e.addPath(m.copy(strokingWithWidth: 24, lineCap: .round, lineJoin: .round, miterLimit: 10))
                    whites(e)
                    ctx.restoreGState()
                case "st_tall":
                    // A capsule body instead of a dome: tall rounded head rising from the bottom edge.
                    mfill(stroke(P(512, 360), P(512, 220), w).union(circ(512, 216, w * 1.6)))
                    let body = CGPath(roundedRect: CGRect(x: 262, y: 340, width: 500, height: 800), cornerWidth: 250, cornerHeight: 250, transform: nil)
                    mfill(body)
                    if !solid { paint(CGPath(roundedRect: CGRect(x: 262 + w, y: 340 + w, width: 500 - 2 * w, height: 800), cornerWidth: 250 - w, cornerHeight: 250 - w, transform: nil), light ? 0xc4c8cf : tile) }
                    let e = CGMutablePath(); e.addPath(pill(430, 560, 84, 160)); e.addPath(pill(594, 560, 84, 160))
                    whites(e)
                case "st_giant":
                    // Face close-up: the head fills the tile, huge eyes with a glint.
                    mfill(stroke(P(512, 300), P(590, 170), ctrl: P(505, 220), w).union(circ(594, 162, w * 1.7)))
                    mfill(circ(512, 990, 700))
                    for x in [370.0, 654] as [CGFloat] {
                        paint(pill(x, 590, 140, 260), 0x000000)
                        paint(circ(x + 22, 510, 24), 0xffffff)
                    }
                case "st_shades":
                    // Cool bot in black sunglasses with a white glint and a smirk.
                    stickUp(); headO()
                    let g = CGMutablePath()
                    g.addRoundedRect(in: CGRect(x: 270, y: 630, width: 220, height: 150), cornerWidth: 60, cornerHeight: 60)
                    g.addRoundedRect(in: CGRect(x: 534, y: 630, width: 220, height: 150), cornerWidth: 60, cornerHeight: 60)
                    g.addRect(CGRect(x: 480, y: 648, width: 64, height: 26))
                    g.addRect(CGRect(x: 150, y: 646, width: 130, height: 26)); g.addRect(CGRect(x: 744, y: 646, width: 130, height: 26))
                    paint(g, 0x000000)
                    paint(stroke(P(310, 740), P(370, 668), 18), 0xffffff, 0.85)
                    paint(stroke(P(574, 740), P(634, 668), 18), 0xffffff, 0.85)
                    let m = CGMutablePath(); m.move(to: P(470, 856)); m.addQuadCurve(to: P(600, 836), control: P(560, 890))
                    paint(m.copy(strokingWithWidth: 26, lineCap: .round, lineJoin: .round, miterLimit: 10), 0x000000)
                case "st_chomp":
                    // Pac-style chomp about to eat a sparkle.
                    let c0 = P(450, 560)
                    mfill(stroke(P(450, 290), P(500, 170), ctrl: P(445, 220), w).union(circ(503, 162, w * 1.5)))
                    let body = CGMutablePath(); body.move(to: c0)
                    body.addArc(center: c0, radius: 280, startAngle: 0.55, endAngle: 2 * .pi - 0.55, clockwise: false)
                    body.closeSubpath()
                    mfill(body)
                    paint(pill(450, 430, 62, 110), 0x000000)
                    paint(sparkle(812, 560, 86), 0xffffff, glow: 0xdfe6ff, blur: 50)
                    for (x, r) in [(700.0, 12.0), (640, 9)] as [(CGFloat, CGFloat)] { paint(circ(x, 560, r), 0xffffff, 0.7) }
                case "st_bounce":
                    // Mid-jump orb with happy eyes, speed lines and a squashed shadow.
                    paint(CGPath(ellipseIn: CGRect(x: 372, y: 850, width: 280, height: 40), transform: nil), 0x2a2c31)
                    for x in [430.0, 512, 594] as [CGFloat] { mfill(stroke(P(x, 740), P(x, 810), 18)) }
                    ctx.saveGState(); ctx.translateBy(x: 0, y: 50)
                    let wob = CGMutablePath(); wob.move(to: P(512, 230)); wob.addCurve(to: P(540, 110), control1: P(470, 180), control2: P(580, 160))
                    mfill(wob.copy(strokingWithWidth: w * 0.8, lineCap: .round, lineJoin: .round, miterLimit: 10).union(circ(540, 104, w * 1.4)))
                    mfill(CGPath(ellipseIn: CGRect(x: 312, y: 220, width: 400, height: 440), transform: nil))
                    for x in [440.0, 584] as [CGFloat] {
                        let a = CGMutablePath(); a.addArc(center: P(x, 450), radius: 44, startAngle: .pi * 1.1, endAngle: .pi * 1.9, clockwise: false)
                        paint(a.copy(strokingWithWidth: 30, lineCap: .round, lineJoin: .round, miterLimit: 10), 0x000000)
                    }
                    let m = CGMutablePath(); m.move(to: P(462, 520)); m.addQuadCurve(to: P(562, 520), control: P(512, 590))
                    m.closeSubpath(); paint(m, 0x000000)
                    ctx.restoreGState()
                case "st_hang":
                    // Dangling from its antenna like a spider, swinging.
                    ctx.saveGState()
                    ctx.translateBy(x: 512, y: 60); ctx.rotate(by: 0.22); ctx.translateBy(x: -512, y: -60)
                    mfill(stroke(P(512, 40), P(512, 330), 16).union(circ(512, 330, w * 1.3)))
                    mfill(circ(512, 590, 250))
                    let e = CGMutablePath(); e.addPath(pill(450, 540, 66, 120)); e.addPath(pill(574, 540, 66, 120))
                    paint(e, 0x000000)
                    paint(circ(512, 680, 30), 0x000000)
                    ctx.restoreGState()
                    for (i, r) in [300.0, 360].enumerated() {
                        let a = CGMutablePath(); a.addArc(center: P(512, 60), radius: r + 280, startAngle: .pi * 0.62, endAngle: .pi * 0.72, clockwise: false)
                        paint(a.copy(strokingWithWidth: 14, lineCap: .round, lineJoin: .round, miterLimit: 10), 0x8a8f98, [0.7, 0.4][i])
                    }
                case "st_hello":
                    // Waving hello: happy eyes and a raised arm with motion marks.
                    stickUp(); headO()
                    mfill(stroke(P(760, 820), P(850, 470), ctrl: P(850, 680), 70).union(circ(852, 440, 72)))
                    for (r, a0) in [(130.0, -0.5), (180, -0.45)] as [(CGFloat, CGFloat)] {
                        let a = CGMutablePath(); a.addArc(center: P(852, 440), radius: r, startAngle: .pi + a0, endAngle: .pi + a0 + 0.6, clockwise: false)
                        mfill(a.copy(strokingWithWidth: 16, lineCap: .round, lineJoin: .round, miterLimit: 10))
                    }
                    for x in [400.0, 580] as [CGFloat] {
                        let a = CGMutablePath(); a.addArc(center: P(x, 760), radius: 54, startAngle: .pi * 1.1, endAngle: .pi * 1.9, clockwise: false)
                        paint(a.copy(strokingWithWidth: 34, lineCap: .round, lineJoin: .round, miterLimit: 10), 0x000000)
                    }
                case "st_love", "st_sleep", "st_think", "st_ghost", "st_ufo", "st_cyclops", "st_duo", "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt":
                    func heart(_ x: CGFloat, _ y: CGFloat, _ k: CGFloat) -> CGPath {
                        let h = CGMutablePath(); h.move(to: P(x, y + k * 0.9))
                        h.addCurve(to: P(x, y - k * 0.35), control1: P(x - k * 1.3, y + k * 0.1), control2: P(x - k * 0.6, y - k * 1.1))
                        h.addCurve(to: P(x, y + k * 0.9), control1: P(x + k * 0.6, y - k * 1.1), control2: P(x + k * 1.3, y + k * 0.1))
                        h.closeSubpath(); return h
                    }
                    func line(_ pts: [CGPoint], _ lw: CGFloat) -> CGPath {
                        let l = CGMutablePath(); l.addLines(between: pts)
                        return l.copy(strokingWithWidth: lw, lineCap: .round, lineJoin: .round, miterLimit: 10)
                    }
                    func arcEye(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat, down: Bool, _ lw: CGFloat) -> CGPath {
                        let a = CGMutablePath()
                        if down { a.addArc(center: P(x, y), radius: r, startAngle: .pi * 0.1, endAngle: .pi * 0.9, clockwise: false) }
                        else { a.addArc(center: P(x, y), radius: r, startAngle: .pi * 1.1, endAngle: .pi * 1.9, clockwise: false) }
                        return a.copy(strokingWithWidth: lw, lineCap: .round, lineJoin: .round, miterLimit: 10)
                    }
                    func smile(_ x: CGFloat, _ y: CGFloat, _ wd: CGFloat, _ lw: CGFloat) -> CGPath {
                        let m = CGMutablePath(); m.move(to: P(x - wd / 2, y)); m.addQuadCurve(to: P(x + wd / 2, y), control: P(x, y + wd * 0.45))
                        return m.copy(strokingWithWidth: lw, lineCap: .round, lineJoin: .round, miterLimit: 10)
                    }
                    func ball(_ x: CGFloat, _ y: CGFloat, _ r: CGFloat, ant: CGFloat = 0.25) {
                        let top = P(x, y - r + 10), tip = P(x + r * ant, y - r - r * 0.45)
                        mfill(stroke(top, tip, ctrl: P(x - 4, y - r - r * 0.25), r * 0.12).union(circ(tip.x, tip.y, r * 0.17)))
                        mfill(circ(x, y, r))
                    }
                    switch variant {
                    case "st_love":
                        // Giant face with heart eyes.
                        mfill(stroke(P(512, 300), P(590, 170), ctrl: P(505, 220), w).union(circ(594, 162, w * 1.7)))
                        mfill(circ(512, 990, 700))
                        paint(heart(370, 580, 110), 0x000000); paint(heart(654, 580, 110), 0x000000)
                        paint(smile(512, 760, 120, 26), 0x000000)
                    case "st_sleep":
                        // Dozing orb with a drooping antenna and floating Zs.
                        mfill(stroke(P(470, 400), P(370, 300), ctrl: P(470, 290), w).union(circ(362, 304, w * 1.5)))
                        mfill(circ(470, 620, 250))
                        paint(arcEye(410, 600, 44, down: true, 28), 0x000000); paint(arcEye(540, 600, 44, down: true, 28), 0x000000)
                        paint(circ(475, 720, 22), 0x000000)
                        for (x, y, k, lw) in [(650.0, 360.0, 110.0, 24.0), (770, 260, 74, 18), (840, 195, 48, 13)] as [(CGFloat, CGFloat, CGFloat, CGFloat)] {
                            mfill(line([P(x, y), P(x + k, y), P(x, y + k), P(x + k, y + k)], lw))
                        }
                    case "st_think":
                        // Looking up at a thought cloud with a sparkle inside.
                        stickUp(400, 400); headO()
                        let e = CGMutablePath(); e.addPath(pill(450, 690, 80, 150)); e.addPath(pill(620, 680, 80, 150))
                        paint(e, 0x000000)
                        paint(circ(470, 650, 18), 0xffffff); paint(circ(640, 640, 18), 0xffffff)
                        mfill(circ(610, 500, 20).union(circ(650, 440, 30)))
                        let cl = CGMutablePath()
                        for (x, y, r) in [(650.0, 290.0, 70.0), (740, 250, 85), (815, 300, 62), (730, 340, 70)] as [(CGFloat, CGFloat, CGFloat)] {
                            cl.addPath(circ(x, y, r))
                        }
                        mfill(cl)
                        paint(sparkle(735, 295, 56), 0x000000)
                    case "st_ghost":
                        // A floating steel ghost bot saying "o".
                        paint(CGPath(ellipseIn: CGRect(x: 382, y: 850, width: 260, height: 36), transform: nil), 0x2a2c31)
                        mfill(stroke(P(512, 230), P(570, 160), ctrl: P(505, 180), w * 0.8).union(circ(574, 156, w * 1.3)))
                        let g = CGMutablePath()
                        g.move(to: P(272, 780)); g.addLine(to: P(272, 450))
                        g.addArc(center: P(512, 450), radius: 240, startAngle: .pi, endAngle: 0, clockwise: false)
                        g.addLine(to: P(752, 780))
                        for i in 0..<3 {
                            let x0 = 752 - 160 * CGFloat(i)
                            g.addQuadCurve(to: P(x0 - 160, 780), control: P(x0 - 80, i % 2 == 0 ? 700 : 860))
                        }
                        g.closeSubpath(); mfill(g)
                        paint(CGPath(ellipseIn: CGRect(x: 380, y: 420, width: 90, height: 130), transform: nil), 0x000000)
                        paint(CGPath(ellipseIn: CGRect(x: 554, y: 420, width: 90, height: 130), transform: nil), 0x000000)
                        paint(CGPath(ellipseIn: CGRect(x: 476, y: 590, width: 72, height: 90), transform: nil), 0x000000)
                    case "st_ufo":
                        // The bot pilots a flying saucer with a tractor beam.
                        let beam = CGMutablePath()
                        beam.move(to: P(420, 620)); beam.addLine(to: P(604, 620)); beam.addLine(to: P(760, 930)); beam.addLine(to: P(264, 930)); beam.closeSubpath()
                        ctx.saveGState(); ctx.addPath(beam); ctx.clip()
                        ctx.drawLinearGradient(gradient([(0xdfe6ff, 0.35, 0), (0xdfe6ff, 0, 1)]), start: P(512, 620), end: P(512, 930), options: [])
                        ctx.restoreGState()
                        mfill(stroke(P(512, 340), P(570, 230), ctrl: P(505, 280), w * 0.8).union(circ(574, 224, w * 1.3)))
                        ctx.saveGState(); ctx.clip(to: CGRect(x: 0, y: 0, width: size, height: 560))
                        mfill(circ(512, 540, 200))
                        let e = CGMutablePath(); e.addPath(pill(456, 460, 52, 96)); e.addPath(pill(568, 460, 52, 96))
                        paint(e, 0x000000)
                        ctx.restoreGState()
                        let saucer = CGPath(ellipseIn: CGRect(x: 172, y: 520, width: 680, height: 150), transform: nil)
                        mfill(saucer)
                        paint(CGPath(ellipseIn: CGRect(x: 172, y: 520, width: 680, height: 80), transform: nil), 0xffffff, 0.12)
                        for x in [260.0, 386, 512, 638, 764] as [CGFloat] { paint(circ(x, 610, 18), 0x000000) }
                    case "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt":
                        // Cyclops forks: giant one-eyed face, each changes the eye or the pose.
                        func face() {
                            mfill(stroke(P(512, 300), P(590, 170), ctrl: P(505, 220), w).union(circ(594, 162, w * 1.7)))
                            mfill(circ(512, 990, 700))
                        }
                        let eye = CGPath(ellipseIn: CGRect(x: 392, y: 450, width: 240, height: 320), transform: nil)
                        switch variant {
                        case "cy_happy":
                            face()
                            paint(arcEye(512, 640, 120, down: false, 60), 0x000000)
                        case "cy_slit":
                            // One wide visor slit with a glowing eye inside, EVE style.
                            face()
                            paint(CGPath(roundedRect: CGRect(x: 250, y: 540, width: 524, height: 150), cornerWidth: 75, cornerHeight: 75, transform: nil), 0x000000)
                            paint(pill(512, 615, 150, 86), 0xffffff, glow: 0xdfe6ff, blur: 50)
                        case "cy_iris":
                            face()
                            paint(eye, 0x000000)
                            ctx.addEllipse(in: CGRect(x: 437, y: 515, width: 150, height: 190))
                            ctx.setStrokeColor(rgb(0xd8dce4)); ctx.setLineWidth(14); ctx.strokePath()
                            paint(circ(560, 540, 30), 0xffffff)
                        case "cy_look":
                            // Eye glances up at its own antenna.
                            face()
                            paint(eye, 0x000000)
                            paint(CGPath(ellipseIn: CGRect(x: 520, y: 470, width: 90, height: 110), transform: nil), 0xffffff)
                        case "cy_orb":
                            // Whole floating ball, one big eye.
                            paint(CGPath(ellipseIn: CGRect(x: 362, y: 840, width: 300, height: 34), transform: nil), 0x2a2c31)
                            ball(512, 560, 250)
                            paint(CGPath(ellipseIn: CGRect(x: 427, y: 445, width: 170, height: 225), transform: nil), 0x000000)
                            paint(circ(553, 502, 26), 0xffffff)
                        case "cy_spark":
                            // The pupil is a white AI sparkle.
                            face()
                            paint(eye, 0x000000)
                            paint(sparkle(512, 610, 96), 0xffffff, glow: 0xdfe6ff, blur: 40)
                        default:
                            // "cy_tilt": peek pose with the one eye.
                            ctx.saveGState()
                            ctx.translateBy(x: 512, y: 1000); ctx.rotate(by: -0.2); ctx.translateBy(x: -512, y: -1000)
                            mfill(stroke(P(512, 420), P(570, 260), ctrl: P(500, 330), w).union(circ(574, 252, w * 1.7)))
                            mfill(circ(512, 1110, 700))
                            paint(CGPath(ellipseIn: CGRect(x: 400, y: 520, width: 230, height: 300), transform: nil), 0x000000)
                            paint(circ(560, 590, 34), 0xffffff)
                            ctx.restoreGState()
                        }
                    case "st_cyclops":
                        // Giant face, one huge eye.
                        mfill(stroke(P(512, 300), P(590, 170), ctrl: P(505, 220), w).union(circ(594, 162, w * 1.7)))
                        mfill(circ(512, 990, 700))
                        paint(CGPath(ellipseIn: CGRect(x: 392, y: 450, width: 240, height: 320), transform: nil), 0x000000)
                        paint(circ(560, 530, 36), 0xffffff)
                    default:
                        // "st_duo": big bot and a little one looking up at it.
                        ball(400, 600, 240)
                        let e = CGMutablePath(); e.addPath(pill(350, 590, 64, 120)); e.addPath(pill(470, 590, 64, 120))
                        paint(e, 0x000000)
                        paint(smile(412, 700, 80, 20), 0x000000)
                        ball(730, 740, 130, ant: -0.3)
                        let f = CGMutablePath(); f.addPath(pill(700, 710, 38, 70)); f.addPath(pill(770, 710, 38, 70))
                        paint(f, 0x000000)
                    }
                default:
                    // "st_peek": the head tilts in from below, eyes glancing up at the antenna.
                    ctx.saveGState()
                    ctx.translateBy(x: 512, y: 1000); ctx.rotate(by: -0.2); ctx.translateBy(x: -512, y: -1000)
                    mfill(stroke(P(512, 560), P(560, 340), ctrl: P(500, 430), w).union(circ(562, 334, w * 1.6)))
                    headO(512, 1170, 620)
                    // Peek cyclops: one eye in place of the pair.
                    switch variant {
                    case "pc_eye": whites(pill(542, 720, 130, 210))
                    case "pc_round": whites(circ(542, 730, 92))
                    case "pc_look":
                        whites(pill(542, 710, 130, 210))
                        paint(circ(566, 660, 24), solid || light ? 0xffffff : 0x000000)
                    case "pc_wide": whites(pill(542, 740, 250, 110))
                    case "pc_happy":
                        whites(stroke(P(452, 770), P(632, 770), ctrl: P(542, 610), 48))
                    case "pc_scan":
                        paint(pill(542, 740, 380, 96), solid || light ? 0x000000 : 0xdfe6ff, 0.18)
                        whites(pill(610, 740, 110, 96))
                    default: whites({ let e = CGMutablePath(); e.addPath(pill(450, 720, 88, 150)); e.addPath(pill(634, 720, 88, 150)); return e }())
                    }
                    ctx.restoreGState()
                }
            case "mf_steel":
                // Grok-style metal: thick min_flat drawn in brushed steel on black. 3rd arg picks the finish.
                let style = CommandLine.arguments.count > 3 ? CommandLine.arguments[3] : "steel"
                let brushed: [(UInt32, CGFloat, CGFloat)] = [(0xf2f4f7, 1, 0), (0xb4b9c2, 1, 0.45), (0x6d727b, 1, 1)]
                let chrome: [(UInt32, CGFloat, CGFloat)] = [(0xffffff, 1, 0), (0xc8ccd4, 1, 0.3), (0x4a4e56, 1, 0.48),
                                                            (0xe6e8ec, 1, 0.6), (0x7d828b, 1, 1)]
                let gun: [(UInt32, CGFloat, CGFloat)] = [(0xa9aeb7, 1, 0), (0x5d6169, 1, 0.5), (0x2e3036, 1, 1)]
                let metal = style == "chrome" ? chrome : style == "gunmetal" ? gun : brushed
                let light = style == "inverse"
                let tile: UInt32 = style == "graphite" ? 0x1b1c20 : 0x000000
                if light {
                    ctx.drawLinearGradient(gradient([(0xeef0f3, 1, 0), (0xb9bec6, 1, 0.6), (0x8d929b, 1, 1)]),
                                           start: P(512, 100), end: P(512, 924), options: [])
                } else {
                    flatBg(tile)
                    // Faint top sheen so the tile reads as glass, not a flat hole.
                    ctx.drawLinearGradient(gradient([(0xffffff, 0.07, 0), (0xffffff, 0, 0.5)]),
                                           start: P(512, 100), end: P(512, 924), options: [])
                }
                func metalFill(_ p: CGPath) {
                    if light { paint(p, 0x0a0a0c); return }
                    ctx.saveGState(); ctx.addPath(p); ctx.clip()
                    ctx.drawLinearGradient(gradient(metal), start: P(512, 300), end: P(512, 924), options: [])
                    ctx.restoreGState()
                }
                let w: CGFloat = 34
                let solidHead = style == "solid"
                let o = CGMutablePath()
                o.addEllipse(in: CGRect(x: -440, y: -80, width: 880, height: 160), transform: tilt(512, 560, -0.12))
                let ringP = o.copy(strokingWithWidth: w, lineCap: .round, lineJoin: .round, miterLimit: 10)
                let gapCol: UInt32 = light ? 0xc4c8cf : tile
                if !solidHead { metalFill(ringP) }
                metalFill(stroke(P(512, 550), P(512, 390), w).union(circ(512, 390, w * 1.6)))
                if solidHead {
                    metalFill(circ(512, 1064, 540))
                } else {
                    metalFill(circ(512, 1064, 540 + w / 2))
                    paint(circ(512, 1064, 540 - w / 2), light ? 0xc4c8cf : tile)
                }
                ctx.saveGState()
                var t = tilt(512, 560, -0.12)
                ctx.addPath(CGPath(rect: CGRect(x: -700, y: 0, width: 1400, height: 600), transform: &t)); ctx.clip()
                ctx.saveGState(); ctx.addPath(circ(512, 1064, 560)); ctx.clip()
                paint(o.copy(strokingWithWidth: w * (solidHead ? 1.2 : 2.6), lineCap: .round, lineJoin: .round, miterLimit: 10), gapCol)
                ctx.restoreGState()
                if !solidHead { metalFill(ringP) }
                ctx.restoreGState()
                if solidHead {
                    // Ring outside the head stays metal; inside it is only the cut.
                    ctx.saveGState()
                    ctx.addPath(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil))
                    ctx.addPath(circ(512, 1064, 560)); ctx.clip(using: .evenOdd)
                    metalFill(ringP)
                    ctx.restoreGState()
                }
                let e = CGMutablePath(); e.addPath(pill(420, 730, 88, 170)); e.addPath(pill(604, 730, 88, 170))
                if solidHead { paint(e, 0x000000) }
                else if light { paint(e, 0xeef0f3) }
                else if style == "chrome" { paint(e, 0xffffff, glow: 0xdfe6ff, blur: 40) }
                else { metalFill(e) }
            case "mf_color":
                // Thick min_flat in a named palette (3rd arg): (tile, line, eyes).
                let pals: [String: (UInt32, UInt32, UInt32)] = [
                    "neon": (0x1a0833, pink, 0x7ff0ff), "ice": (0x0a1430, 0x7fd8ff, 0xffffff),
                    "ember": (0x1c0904, 0xff6a1a, 0xffd36b), "toxic": (0x06140c, 0x3dff8a, 0xeaffef),
                    "gold": (0x0d0b08, 0xffc23d, 0xffffff), "mono": (0x000000, 0xffffff, 0xffffff),
                    "sakura": (0x2a0e1e, 0xffa8cc, 0xffffff), "violet": (0x0e0a24, 0x9b7bff, 0x7ff0ff),
                    "red": (0x0a0a0a, 0xff2d3d, 0xffffff),
                ]
                let (bg, line, eye) = pals[CommandLine.arguments.count > 3 ? CommandLine.arguments[3] : "neon"] ?? pals["neon"]!
                flatBg(bg)
                minFlat(bg: bg, line: line, eye: eye, w: 34, eyeW: 88, eyeH: 170)
            case "mf_thick":
                // Heavier strokes and bigger eyes so it holds at Dock size.
                flatBg(0x1a0833)
                minFlat(bg: 0x1a0833, line: pink, eye: 0x7ff0ff, w: 34, eyeW: 88, eyeH: 170)
            case "mf_solid":
                // No outline: the head is a flat lighter-purple silhouette.
                flatBg(0x1a0833)
                minFlat(bg: 0x1a0833, line: pink, eye: 0x7ff0ff, fill: 0x351766, outline: false)
            case "mf_pink":
                // Single colour: everything pink on deep purple.
                flatBg(0x1a0833)
                minFlat(bg: 0x1a0833, line: pink, eye: pink)
            case "mf_black":
                // Same drawing on a pure black tile.
                flatBg(0x000000)
                minFlat(bg: 0x000000, line: pink, eye: 0x7ff0ff, fill: 0x000000)
            case "mf_tilt":
                // Steeper ring, more motion.
                flatBg(0x1a0833)
                minFlat(bg: 0x1a0833, line: pink, eye: 0x7ff0ff, w: 26, tilt: -0.32, eyeY: 750, ringY: 600)
            case "mf_dusk":
                // Sky fades from deep purple into magenta behind the head.
                ctx.drawLinearGradient(gradient([(0x12052a, 1, 0), (0x2a0a4a, 1, 0.4), (0x8a1670, 1, 0.75)]),
                                       start: P(512, 100), end: P(512, 924), options: [])
                minFlat(bg: 0x1a0833, line: pink, eye: 0x7ff0ff, w: 26)
            case "min_flat":
                // Neonsat flattened: purple tile, one pink ring, outlined head, cyan eyes.
                flatBg(0x1a0833)
                flatRing(512, 560, 440, 80, -0.12, 22, pink, gap: ink, gapClip: circ(512, 1064, 560)) {
                    stick(512, 540, 150, pink, 22, tip: pink)
                    outlineHead(512, 1064, 540, pink, 22)
                }
                let e = CGMutablePath(); e.addPath(pill(420, 730, 74, 150)); e.addPath(pill(604, 730, 74, 150))
                paint(e, 0x7ff0ff)
            case "min_mono":
                // Pure white on black, Grok style.
                flatBg(0x000000)
                flatRing(512, 560, 440, 80, -0.12, 22, 0xffffff) {
                    stick(512, 540, 150, 0xffffff, 22, tip: 0xffffff)
                    outlineHead(512, 1064, 540, 0xffffff, 22)
                }
                let e = CGMutablePath(); e.addPath(pill(420, 730, 74, 150)); e.addPath(pill(604, 730, 74, 150))
                paint(e, 0xffffff)
            case "min_glyph":
                // Whole planet glyph: solid white disc, ring cut through it, two black eyes.
                flatBg(0x000000)
                flatRing(512, 540, 380, 96, -0.3, 26, 0xffffff) {
                    paint(circ(512, 540, 240), 0x000000)
                    paint(circ(512, 540, 220), 0xffffff)
                }
                let e = CGMutablePath(); e.addPath(pill(456, 520, 48, 104)); e.addPath(pill(568, 520, 48, 104))
                paint(e, 0x000000)
                paint(stroke(P(512, 320), P(512, 230), 20), 0xffffff); paint(circ(512, 222, 30), 0xffffff)
            case "min_solid":
                // Solid pink head, ring drawn as a black gap through it.
                flatBg(0x0b0216)
                stick(512, 540, 150, pink, 22, tip: pink)
                paint(circ(512, 1064, 540), pink)
                let o = CGMutablePath()
                o.addEllipse(in: CGRect(x: -440, y: -80, width: 880, height: 160), transform: tilt(512, 560, -0.12))
                ctx.saveGState(); ctx.addPath(circ(512, 1064, 560)); ctx.clip()
                paint(o.copy(strokingWithWidth: 40, lineCap: .round, lineJoin: .round, miterLimit: 10), 0x0b0216)
                ctx.restoreGState()
                ctx.saveGState()
                ctx.addPath(CGPath(rect: CGRect(x: 0, y: 0, width: size, height: size), transform: nil))
                ctx.addPath(circ(512, 1064, 560)); ctx.clip(using: .evenOdd)
                paint(o.copy(strokingWithWidth: 22, lineCap: .round, lineJoin: .round, miterLimit: 10), pink)
                ctx.restoreGState()
                let e = CGMutablePath(); e.addPath(pill(420, 760, 74, 140)); e.addPath(pill(604, 760, 74, 140))
                paint(e, 0x0b0216)
            default:
                // "min_duo": neonsat palette, flat shapes, one soft glow on the eyes only.
                synthSky()
                flatRing(512, 560, 440, 80, -0.12, 20, 0xff6ad5, gap: ink, gapClip: circ(512, 1064, 560)) {
                    stick(512, 540, 150, ink, 22, tip: 0xffd0f0)
                    paint(circ(512, 1064, 548), 0xffd0f0)
                    paint(circ(512, 1064, 540), ink)
                }
                lit([(420, 730), (604, 730)], 74, 150, 0xbff6ff, cyan)
            }
        // Forks of neonsat (ns_*) and sunsat (ss_*).
        case "ns_cyan":
            // Neonsat with colours swapped: cyan rim and ring on deep blue, pink eyes.
            ctx.drawLinearGradient(gradient([(0x020617, 1, 0), (0x0b2350, 1, 0.65), (0x02040c, 1, 0.66)]),
                                   start: P(512, 100), end: P(512, 924), options: [])
            stars(sky)
            ringed(512, 520, 470, 88, -0.12, 0x9ff0ff, cyan) {
                antenna(512, 540, 1, cyan, tip: 0xd8f8ff)
                head(512, 1064, 540, cyan, rim: 0xd8f8ff)
            }
            lit([(410, 720), (614, 720)], 76, 150, 0xffc0e8, pink)
        case "ns_atom":
            // Two crossed rings, pink and cyan, like an atom.
            synthSky(); stars(sky, 0xff6ad5)
            ringed(512, 520, 470, 88, -0.14, 0xff8ad8, pink) {
                ringed(512, 540, 440, 76, 0.26, 0x9ff0ff, cyan) {
                    antenna(512, 540, 1, pink, tip: 0xffd0f0)
                    head(512, 1064, 540, pink, rim: 0xffd0f0)
                }
            }
            lit([(410, 740), (614, 740)], 76, 140, 0xbff6ff, cyan)
        case "ns_wire":
            // Neonsat with a faint wireframe globe on the head.
            synthSky(); stars(sky, 0xff6ad5)
            ringed(512, 520, 470, 88, -0.12, 0xff8ad8, pink) {
                antenna(512, 540, 1, pink, tip: 0xffd0f0)
                head(512, 1064, 540, pink, rim: 0xffd0f0)
                ctx.saveGState(); ctx.addPath(circ(512, 1064, 534)); ctx.clip()
                ctx.setStrokeColor(rgb(0xff6ad5, 0.35)); ctx.setLineWidth(4)
                for k in [0.3, 0.62, 0.9] as [CGFloat] {
                    ctx.addEllipse(in: CGRect(x: 512 - 540 * k, y: 524, width: 1080 * k, height: 1080))
                }
                ctx.move(to: P(512, 524)); ctx.addLine(to: P(512, 924))
                for y in [600.0, 700, 820] as [CGFloat] {
                    let half = sqrt(max(0, 540 * 540 - (1064 - y) * (1064 - y)))
                    ctx.addEllipse(in: CGRect(x: 512 - half, y: y - 18, width: 2 * half, height: 36))
                }
                ctx.strokePath(); ctx.restoreGState()
            }
            lit([(410, 720), (614, 720)], 76, 150, 0xbff6ff, cyan)
        case "ss_dusk":
            // Sunsat moved into the synth sky with a pink ring.
            synthSky(); stars(sky, 0xff6ad5)
            ringed(512, 520, 470, 88, -0.12, 0xff8ad8, pink) {
                antenna(512, 540, 1, pink, tip: 0xffe46b)
                sunFill(circ(512, 1064, 540), 524, 924, stripes: [820, 856, 890])
            }
            lit([(410, 690), (614, 690)], 76, 150, 0x1a0630, pink)
        case "ss_ice":
            // Sunsat as a cold star: cyan into violet.
            stars(sky)
            ringed(512, 520, 470, 88, -0.12, 0xffffff, ice) {
                antenna(512, 540, 1, cyan, tip: 0xd8f8ff)
                sunFill(circ(512, 1064, 540), 524, 924, stripes: [820, 856, 890],
                        colors: [(0xd8f8ff, 1, 0), (0x6fa8ff, 1, 0.45), (0x7a3dff, 1, 1)], glow: cyan)
            }
            lit([(410, 690), (614, 690)], 76, 150, 0x0a0630, cyan)
        case "ss_moon":
            // Sunsat with a little moon riding the ring.
            stars(sky)
            ringed(512, 520, 470, 88, -0.12, 0xffffff, ice) {
                antenna(512, 540, 1, pink, tip: 0xffe46b)
                sunFill(circ(512, 1064, 540), 524, 924, stripes: [820, 856, 890])
            }
            let m = CGPoint(x: 470 * cos(2.3), y: 88 * sin(2.3)).applying(tilt(512, 520, -0.12))
            paint(circ(m.x, m.y, 40), 0xffffff, glow: ice, blur: 50)
            paint(circ(m.x - 12, m.y + 10, 30), 0xd8dcea)
            lit([(410, 690), (614, 690)], 76, 150, 0x1a0630, pink)
        default: break
        }
        if !["neonring", "neonsat", "nm_clean", "nm_bare", "nm_cyclops", "nm_orb", "nm_black", "nm_thin", "nc_happy", "nc_look", "nc_blink", "nc_scan", "nc_tilt", "nc_peek", "nc_heart", "nc_sleep", "nc_zoom", "mc_noring", "mc_dome", "mc_flat", "mc_line", "mc_black", "mc_pill", "mh_flat", "mh_black", "mh_mono", "mh_level", "mh_line", "mh_soft", "sunsat", "planet", "gridsat", "ns_cyan", "ns_atom", "ns_wire", "ss_dusk", "ss_ice", "ss_moon", "min_flat", "min_mono", "min_glyph", "min_solid", "min_duo", "mf_thick", "mf_solid", "mf_pink", "mf_black", "mf_tilt", "mf_dusk", "mf_color", "mf_steel", "st_visor", "st_spark", "st_chat", "st_wink", "st_twin", "st_peek", "st_orb", "st_smile", "st_tall", "st_giant", "st_shades", "st_chomp", "st_bounce", "st_hang", "st_hello", "st_love", "st_sleep", "st_think", "st_ghost", "st_ufo", "st_cyclops", "st_duo", "cy_happy", "cy_slit", "cy_iris", "cy_look", "cy_orb", "cy_spark", "cy_tilt", "pc_eye", "pc_round", "pc_look", "pc_wide", "pc_happy", "pc_scan"].contains(variant) {
        switch variant {
        case "aurora":
            // Northern lights curtains over the rising head, green into violet.
            ctx.drawLinearGradient(gradient([(0x02060f, 1, 0), (0x061a24, 1, 0.6), (0x02040a, 1, 1)]),
                                   start: P(512, 100), end: P(512, 924), options: [])
            for (i, (y0, amp, hue)) in [(300.0, 70.0, 0x3dffa8 as UInt32), (380, 90, 0x7a5cff), (240, 50, 0x00e0ff)].enumerated() {
                let w = CGMutablePath(); w.move(to: P(60, y0))
                for k in 1...6 {
                    let x = 60 + CGFloat(k) * 160
                    w.addQuadCurve(to: P(x, y0 + (k % 2 == 0 ? 0 : amp * 0.4)), control: P(x - 80, y0 + (k % 2 == 0 ? amp : -amp)))
                }
                ctx.saveGState()
                ctx.addPath(w.copy(strokingWithWidth: [150, 110, 80][i], lineCap: .round, lineJoin: .round, miterLimit: 10)); ctx.clip()
                ctx.drawLinearGradient(gradient([(hue, 0, 0), (hue, 0.4, 0.6), (0xffffff, 0.35, 0.85), (hue, 0, 1)]),
                                       start: P(512, y0 - 90), end: P(512, y0 + 90), options: [])
                ctx.restoreGState()
            }
            stars(sky.filter { $0.1 < 200 || $0.1 > 400 })
            antenna(512, 540, 1, 0x3dffa8, tip: 0xd8fff0)
            head(512, 1064, 540, 0x3dffa8, rim: 0xe0fff2)
            lit([(410, 690), (614, 690)], 76, 160, 0xd8fff0, 0x3dffa8)
        case "synth":
            // Synthwave: the head is a striped sunset sun sinking into a neon grid.
            ctx.drawLinearGradient(gradient([(0x0b0220, 1, 0), (0x2a0a4a, 1, 0.45), (0x7a1268, 1, 0.66), (0x0b0220, 1, 0.67)]),
                                   start: P(512, 100), end: P(512, 924), options: [])
            stars(sky.filter { $0.1 < 330 }, 0xff6ad5)
            let horizon: CGFloat = 650
            paint(stroke(P(512, 300), P(600, 160), ctrl: P(505, 220), 26), 0xff6ad5, glow: 0xff2e88, blur: 30)
            paint(circ(602, 154, 38), 0xffffff, glow: 0xff2e88, blur: 60)
            ctx.saveGState()
            ctx.clip(to: CGRect(x: 0, y: 0, width: size, height: horizon))
            let sun = circ(512, 540, 250)
            paint(sun, 0xffffff, glow: 0xff2e88, blur: 120)
            ctx.addPath(sun); ctx.clip()
            ctx.drawLinearGradient(gradient([(0xffe46b, 1, 0), (0xff8a3d, 1, 0.5), (0xff2e88, 1, 1)]),
                                   start: P(512, 290), end: P(512, 650), options: [])
            ctx.setBlendMode(.clear)
            for (i, y) in [560.0, 594, 624].enumerated() { ctx.fill(CGRect(x: 0, y: y, width: size, height: [8.0, 12, 16][i])) }
            ctx.setBlendMode(.normal)
            lit([(430, 470), (594, 470)], 62, 130, 0x1a0630, 0xff2e88)
            ctx.restoreGState()
            ctx.saveGState()
            ctx.clip(to: CGRect(x: 0, y: horizon, width: size, height: size - horizon))
            ctx.setStrokeColor(rgb(0xff3fc0, 0.85)); ctx.setLineWidth(5)
            ctx.setShadow(offset: .zero, blur: 18, color: rgb(0xff2e88, 1))
            for i in -8...8 {
                ctx.move(to: P(512 + CGFloat(i) * 40, horizon)); ctx.addLine(to: P(512 + CGFloat(i) * 230, size))
            }
            var y = horizon + 10, step: CGFloat = 14
            while y < size { ctx.move(to: P(0, y)); ctx.addLine(to: P(size, y)); step *= 1.45; y += step }
            ctx.strokePath()
            ctx.restoreGState()
            paint(CGPath(rect: CGRect(x: 0, y: horizon - 3, width: size, height: 6), transform: nil), 0xffd0f0, glow: 0xff2e88, blur: 30)
        case "portal":
            // The bot climbs out of a glowing portal hanging in space.
            stars(sky)
            let gate = circ(512, 470, 300)
            paint(gate, 0x0a0f24)
            ctx.saveGState(); ctx.addPath(gate); ctx.clip()
            ctx.drawRadialGradient(gradient([(0x2b3d8a, 1, 0), (0x0a0f24, 1, 0.7), (0x03040a, 1, 1)]),
                                   startCenter: P(512, 470), startRadius: 0, endCenter: P(512, 470), endRadius: 300, options: [])
            stars([(420, 300, 3), (600, 260, 2.5), (380, 450, 2), (650, 400, 3), (520, 340, 2)])
            ctx.restoreGState()
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: 90, color: rgb(0x7fb2ff, 1))
            ctx.addPath(gate); ctx.setLineWidth(22); ctx.setStrokeColor(rgb(0xffffff)); ctx.strokePath()
            ctx.restoreGState()
            // Head pokes out through the bottom of the ring, over its rim.
            antenna(512, 560, 0.8, ice)
            head(512, 880, 330, ice)
            lit([(445, 680), (579, 680)], 62, 130, 0xffffff, ice)
        case "saturn":
            // Rise head wearing a tilted ring, half behind it, half in front.
            stars(sky)
            func ringPath() -> CGPath {
                var t = CGAffineTransform(translationX: 512, y: 520).rotated(by: -0.12)
                let o = CGMutablePath()
                o.addEllipse(in: CGRect(x: -470, y: -88, width: 940, height: 176), transform: t)
                return o.copy(strokingWithWidth: 26, lineCap: .round, lineJoin: .round, miterLimit: 10)
                    .union(CGPath(ellipseIn: CGRect(x: -420, y: -66, width: 840, height: 132), transform: &t)
                        .copy(strokingWithWidth: 8, lineCap: .round, lineJoin: .round, miterLimit: 10))
            }
            paint(ringPath(), 0xdfe6ff, 0.55, glow: ice, blur: 40)
            antenna(512, 540, 1, ice)
            head(512, 1064, 540, ice)
            ctx.saveGState()
            var t = CGAffineTransform(translationX: 512, y: 520).rotated(by: -0.12)
            ctx.addPath(CGPath(rect: CGRect(x: -600, y: 0, width: 1200, height: 400), transform: &t)); ctx.clip()
            paint(ringPath(), 0xffffff, glow: ice, blur: 40)
            ctx.restoreGState()
            lit([(410, 720), (614, 720)], 76, 150, 0xffffff, ice)
        default:
            // "lunar": flipped scale, a small bot head rising over a grey moon horizon.
            stars(sky + [(150, 600, 2.5), (880, 600, 3), (640, 150, 2)])
            antenna(560, 448, 0.55, ice)
            head(560, 640, 190, ice)
            lit([(505, 610), (615, 610)], 46, 96, 0xffffff, ice)
            let ground = circ(512, 1560, 880)
            ctx.saveGState()
            ctx.setShadow(offset: .zero, blur: 50, color: rgb(0xdfe6ff, 0.5))
            ctx.addPath(ground); ctx.setFillColor(rgb(0x2a2c33)); ctx.fillPath()
            ctx.restoreGState()
            ctx.saveGState(); ctx.addPath(ground); ctx.clip()
            ctx.drawLinearGradient(gradient([(0x9aa0ad, 1, 0), (0x3a3d46, 1, 0.25), (0x121318, 1, 1)]),
                                   start: P(512, 680), end: P(512, 924), options: [])
            for (x, y, w, h) in [(260.0, 730.0, 120.0, 30.0), (640, 760, 170, 40), (440, 830, 90, 26), (760, 860, 120, 34), (180, 860, 140, 40)] {
                ctx.addEllipse(in: CGRect(x: x - w / 2, y: y - h / 2, width: w, height: h))
                ctx.setFillColor(rgb(0x000000, 0.35)); ctx.fillPath()
                ctx.addEllipse(in: CGRect(x: x - w / 2, y: y - h / 2 + 4, width: w, height: h))
                ctx.setStrokeColor(rgb(0xffffff, 0.12)); ctx.setLineWidth(3); ctx.strokePath()
            }
            ctx.restoreGState()
        }
        }
    default:
        // "signal": small moon bot broadcasting radio waves from its antenna tip.
        let tip = P(580, 320)
        paint(stroke(P(470, 450), tip, ctrl: P(470, 340), 22), ink, glow: ice, blur: 26)
        for (i, r) in [66.0, 114, 162].enumerated() {
            let arc = CGMutablePath()
            arc.addArc(center: tip, radius: r, startAngle: -1.45, endAngle: 0.25, clockwise: false)
            paint(arc.copy(strokingWithWidth: 20, lineCap: .round, lineJoin: .round, miterLimit: 10),
                  0xffffff, [0.95, 0.6, 0.3][i], glow: ice, blur: 30)
        }
        glowFill(circ(tip.x, tip.y, 34))
        moon(450, 640, 220)
        eyes([(382, 655), (518, 632)], 50, 104)
    }
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
    case "diamond", "crescent", "corona", "blood", "horizon", "eyes", "wink", "grin", "peek", "happy",
         "antenna", "sparkle", "sleepy", "cheeky", "visor2":
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
        let faces = ["eyes", "wink", "grin", "peek", "happy", "antenna", "sparkle", "sleepy", "cheeky", "visor2"]
        if variant == "crescent" || faces.contains(variant) {
            // Moon slid toward the bottom left: only a crescent of light on the top right.
            // "grin" slides it up instead, so the crescent becomes a smile under the eyes.
            let shift = ["grin", "cheeky"].contains(variant) ? CGPoint(x: 0, y: -34) : CGPoint(x: -26, y: 26)
            ctx.addPath(disc(240).copy(using: [CGAffineTransform(translationX: shift.x, y: shift.y)])!)
            ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
            if faces.contains(variant) {
                // Bot face on the moon: glowing ice eyes, offsets from the moon's centre.
                let m = CGPoint(x: c.x + shift.x, y: c.y + shift.y)
                func at(_ dx: CGFloat, _ dy: CGFloat) -> CGPoint { CGPoint(x: m.x + dx, y: m.y + dy) }
                func pill(_ p: CGPoint, _ w: CGFloat, _ h: CGFloat) -> CGPath {
                    CGPath(roundedRect: CGRect(x: p.x - w / 2, y: p.y - h / 2, width: w, height: h),
                           cornerWidth: w / 2, cornerHeight: min(w, h) / 2, transform: nil)
                }
                let eyes = CGMutablePath()
                // Closed eye: a thick round-capped arc, `up` = ^ (happy), else a ‿ wink.
                func arcEye(_ p: CGPoint, up: Bool) {
                    let line = CGMutablePath()
                    line.move(to: CGPoint(x: p.x - 44, y: p.y + (up ? 18 : -10)))
                    line.addQuadCurve(to: CGPoint(x: p.x + 44, y: p.y + (up ? 18 : -10)),
                                      control: CGPoint(x: p.x, y: p.y + (up ? -40 : 34)))
                    eyes.addPath(line.copy(strokingWithWidth: 26, lineCap: .round, lineJoin: .round, miterLimit: 10))
                }
                switch variant {
                case "eyes":
                    eyes.addPath(pill(at(-72, -10), 46, 104)); eyes.addPath(pill(at(72, -10), 46, 104))
                case "wink":
                    eyes.addPath(pill(at(-72, -10), 46, 104)); arcEye(at(76, -10), up: false)
                case "grin":
                    eyes.addPath(pill(at(-80, 0), 58, 58)); eyes.addPath(pill(at(80, 0), 58, 58))
                case "peek":
                    // Eyes glancing up at the light on the top right.
                    eyes.addPath(pill(at(30, -60), 40, 84)); eyes.addPath(pill(at(130, -30), 40, 84))
                case "antenna":
                    // Little bot antenna poking through the rim, glowing tip catching the light.
                    let stem = CGMutablePath()
                    stem.move(to: at(10, -200)); stem.addQuadCurve(to: at(70, -318), control: at(10, -290))
                    ctx.saveGState()
                    ctx.addPath(stem.copy(strokingWithWidth: 22, lineCap: .round, lineJoin: .round, miterLimit: 10))
                    ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
                    ctx.restoreGState()
                    eyes.addPath(pill(at(70, -322), 54, 54))
                    eyes.addPath(pill(at(-70, 0), 48, 96)); eyes.addPath(pill(at(70, 0), 48, 96))
                case "sparkle":
                    // Star-struck: four-point sparkles for eyes.
                    for x in [-78.0, 78.0] {
                        let p = at(x, -6), r = 62.0, w = 16.0
                        let star = CGMutablePath()
                        star.move(to: CGPoint(x: p.x, y: p.y - r))
                        star.addQuadCurve(to: CGPoint(x: p.x + r, y: p.y), control: CGPoint(x: p.x + w, y: p.y - w))
                        star.addQuadCurve(to: CGPoint(x: p.x, y: p.y + r), control: CGPoint(x: p.x + w, y: p.y + w))
                        star.addQuadCurve(to: CGPoint(x: p.x - r, y: p.y), control: CGPoint(x: p.x - w, y: p.y + w))
                        star.addQuadCurve(to: CGPoint(x: p.x, y: p.y - r), control: CGPoint(x: p.x - w, y: p.y - w))
                        eyes.addPath(star)
                    }
                case "sleepy":
                    // Dozing bot: closed eyes, z's drifting up into the light.
                    arcEye(at(-76, 0), up: false); arcEye(at(76, 0), up: false)
                    for (dx, dy, k) in [(170.0, -150.0, 1.0), (245.0, -250.0, 0.7)] {
                        let z = CGMutablePath(), h = 70 * k
                        z.move(to: at(dx - h / 2, dy - h / 2)); z.addLine(to: at(dx + h / 2, dy - h / 2))
                        z.addLine(to: at(dx - h / 2, dy + h / 2)); z.addLine(to: at(dx + h / 2, dy + h / 2))
                        eyes.addPath(z.copy(strokingWithWidth: 18 * k, lineCap: .round, lineJoin: .round, miterLimit: 10))
                    }
                case "cheeky":
                    // Grin face with a wink: crescent smile below, one dot eye, one ‿.
                    eyes.addPath(pill(at(-80, 0), 58, 58)); arcEye(at(80, 4), up: true)
                case "visor2":
                    // Glowing visor band with two dark eye slots, helmet-bot style.
                    eyes.addPath(pill(at(0, -10), 330, 92))
                default:
                    arcEye(at(-76, -6), up: true); arcEye(at(76, -6), up: true)
                }
                glowFill(eyes)
                if variant == "visor2" {
                    ctx.addPath(pill(at(-62, -10), 40, 56)); ctx.addPath(pill(at(62, -10), 40, 56))
                    ctx.setFillColor(rgb(0x050506)); ctx.fillPath()
                }
            }
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

// Vignette so the tile reads as a solid object (skipped on the light tile).
if variant != "frost" { ctx.drawRadialGradient(gradient([(0x000000, 0, 0.6), (0x000000, 0.5, 1)]),
                       startCenter: CGPoint(x: 512, y: 470), startRadius: 0,
                       endCenter: CGPoint(x: 512, y: 470), endRadius: 660, options: []) }
ctx.restoreGState()

// Glass rim: thin top highlight.
ctx.saveGState()
ctx.addPath(squircle); ctx.clip()
// "edge" finish: a brighter, wider rim so a dark tile keeps its outline on a dark Dock.
let edge = CommandLine.arguments.count > 3 && CommandLine.arguments[3] == "edge"
ctx.addPath(CGPath(roundedRect: body.insetBy(dx: edge ? 3 : 1.5, dy: edge ? 3 : 1.5), cornerWidth: 184, cornerHeight: 184, transform: nil))
if edge { ctx.setLineWidth(6) }
ctx.replacePathWithStrokedPath(); ctx.clip()
ctx.drawLinearGradient(gradient(edge ? [(0xffffff, 0.55, 0), (0xffffff, 0.2, 0.5), (0xffffff, 0.3, 1)]
                                     : [(0xffffff, 0.28, 0), (0xffffff, 0.03, 0.5), (0xffffff, 0.08, 1)]),
                       start: CGPoint(x: 0, y: 100), end: CGPoint(x: 0, y: 924), options: [])
ctx.restoreGState()

let out = URL(fileURLWithPath: CommandLine.arguments[1])
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, ctx.makeImage()!, nil)
CGImageDestinationFinalize(dest)
