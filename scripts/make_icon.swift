// Dessine l'icône de l'app (1024×1024) : fond métal sombre, bulle orange avec
// symbole lecture, égaliseur vert façon écran LCD.
// Usage : swift scripts/make_icon.swift sortie.png
import AppKit

let size: CGFloat = 1024
let out = CommandLine.arguments.count > 1 ? CommandLine.arguments[1] : "icon.png"

let rep = NSBitmapImageRep(bitmapDataPlanes: nil, pixelsWide: Int(size), pixelsHigh: Int(size),
                           bitsPerSample: 8, samplesPerPixel: 4, hasAlpha: true, isPlanar: false,
                           colorSpaceName: .deviceRGB, bytesPerRow: 0, bitsPerPixel: 0)!
NSGraphicsContext.saveGraphicsState()
NSGraphicsContext.current = NSGraphicsContext(bitmapImageRep: rep)
let ctx = NSGraphicsContext.current!.cgContext

func rgb(_ hex: UInt32, _ a: CGFloat = 1) -> NSColor {
    NSColor(srgbRed: CGFloat((hex >> 16) & 0xff) / 255, green: CGFloat((hex >> 8) & 0xff) / 255,
            blue: CGFloat(hex & 0xff) / 255, alpha: a)
}

// Grille macOS : le carré arrondi occupe ~80 % de la toile avec une ombre portée.
let inset: CGFloat = 100
let body = NSRect(x: inset, y: inset + 10, width: size - 2 * inset, height: size - 2 * inset)
let radius: CGFloat = 185
let squircle = NSBezierPath(roundedRect: body, xRadius: radius, yRadius: radius)

ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -14), blur: 36, color: rgb(0x000000, 0.45).cgColor)
rgb(0x1b1c21).setFill()
squircle.fill()
ctx.restoreGState()

// Métal brossé : dégradé vertical + fines stries.
ctx.saveGState()
squircle.addClip()
NSGradient(colors: [rgb(0x4a4e58), rgb(0x2a2c33), rgb(0x16171b)], atLocations: [0, 0.55, 1],
           colorSpace: .sRGB)!.draw(in: body, angle: -90)
for i in stride(from: body.minY, to: body.maxY, by: 6) {
    rgb(0xffffff, i.truncatingRemainder(dividingBy: 12) == 0 ? 0.025 : 0.012).setFill()
    NSRect(x: body.minX, y: i, width: body.width, height: 2).fill()
}
// Liseré de titre façon lecteur rétro.
let bar = NSRect(x: body.minX, y: body.maxY - 92, width: body.width, height: 92)
NSGradient(colors: [rgb(0x2c3f7a), rgb(0x131c42)], atLocations: [0, 1], colorSpace: .sRGB)!.draw(in: bar, angle: -90)
for k in 0..<4 {
    rgb(0xc9a24a, 0.55).setFill()
    NSRect(x: body.minX + 70, y: bar.minY + 26 + CGFloat(k) * 12, width: body.width - 140, height: 4).fill()
}
ctx.restoreGState()

// Bord biseauté.
rgb(0xffffff, 0.18).setStroke()
let bevel = NSBezierPath(roundedRect: body.insetBy(dx: 3, dy: 3), xRadius: radius - 3, yRadius: radius - 3)
bevel.lineWidth = 6
bevel.stroke()

// Bulle de dialogue orange (sous-titres) : corps arrondi + queue, remplis d'un seul tenant.
let bubbleRect = NSRect(x: 215, y: 400, width: 594, height: 350)
let bubble = NSBezierPath(roundedRect: bubbleRect, xRadius: 150, yRadius: 150)
let tail = NSBezierPath()
tail.move(to: NSPoint(x: 330, y: 430))
tail.curve(to: NSPoint(x: 270, y: 335), controlPoint1: NSPoint(x: 320, y: 390), controlPoint2: NSPoint(x: 300, y: 355))
tail.curve(to: NSPoint(x: 440, y: 405), controlPoint1: NSPoint(x: 340, y: 345), controlPoint2: NSPoint(x: 400, y: 370))
tail.close()
ctx.saveGState()
ctx.setShadow(offset: CGSize(width: 0, height: -10), blur: 24, color: rgb(0xff8a1f, 0.45).cgColor)
let fill = NSGradient(colors: [rgb(0xffb163), rgb(0xff8a1f), rgb(0xb35a0b)], atLocations: [0, 0.5, 1], colorSpace: .sRGB)!
fill.draw(in: tail, angle: -90)
fill.draw(in: bubble, angle: -90)
ctx.restoreGState()
rgb(0xffd2a6, 0.7).setStroke()
bubble.lineWidth = 5
bubble.stroke()

// Petit symbole lecture à gauche + deux lignes de sous-titres.
let play = NSBezierPath()
play.move(to: NSPoint(x: 330, y: 500))
play.line(to: NSPoint(x: 330, y: 650))
play.line(to: NSPoint(x: 455, y: 575))
play.close()
rgb(0x141414).setFill()
play.fill()
for (i, w) in [CGFloat(250), 190].enumerated() {
    let line = NSBezierPath(roundedRect: NSRect(x: 500, y: 600 - CGFloat(i) * 80, width: w, height: 42), xRadius: 21, yRadius: 21)
    rgb(i == 0 ? 0xffffff : 0xffe7a8, 0.95).setFill()
    line.fill()
}

// Écran LCD avec égaliseur vert (clin d'œil aux lecteurs audio rétro).
let lcd = NSRect(x: 230, y: 165, width: 564, height: 130)
let lcdPath = NSBezierPath(roundedRect: lcd, xRadius: 18, yRadius: 18)
rgb(0x050806).setFill()
lcdPath.fill()
rgb(0x000000).setStroke()
lcdPath.lineWidth = 4
lcdPath.stroke()
let heights: [CGFloat] = [0.45, 0.75, 0.95, 0.6, 0.85, 0.5, 0.7, 0.9, 0.55, 0.35, 0.65, 0.8]
let bw: CGFloat = 36, gap: CGFloat = 9.5
for (i, h) in heights.enumerated() {
    let x = lcd.minX + 22 + CGFloat(i) * (bw + gap)
    let maxH = lcd.height - 36
    var y = lcd.minY + 18
    while y < lcd.minY + 18 + maxH * h {
        let r = (y - lcd.minY - 18) / maxH
        (r > 0.8 ? rgb(0xff5a4f) : r > 0.55 ? rgb(0xffd23d) : rgb(0x3dff8a)).setFill()
        NSRect(x: x, y: y, width: bw, height: 7).fill()
        y += 11
    }
}

NSGraphicsContext.restoreGraphicsState()
try! rep.representation(using: .png, properties: [:])!.write(to: URL(fileURLWithPath: out))
print("icône écrite : \(out)")
