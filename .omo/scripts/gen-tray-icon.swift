// gen-tray-icon.swift
// Generates icons/tray-icon.png: a 32x32 monochrome macOS menu bar template icon.
// Draws an ALAS-themed ship glyph (hull + mast + sail + pennant) in pure black.
// Template-icon contract: every non-transparent pixel MUST be RGB (0,0,0).
// The script self-verifies this over the raw RGBA buffer before writing the PNG
// and fails (non-zero exit) on any colored/gray pixel or an empty icon.
//
// Run from the repo root: swift .omo/scripts/gen-tray-icon.swift

import Foundation
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

let size = 32
let bytesPerRow = size * 4

var pixels = [UInt8](repeating: 0, count: size * size * 4)

let colorSpace = CGColorSpaceCreateDeviceRGB()
guard let ctx = CGContext(
    data: &pixels,
    width: size,
    height: size,
    bitsPerComponent: 8,
    bytesPerRow: bytesPerRow,
    space: colorSpace,
    // premultipliedLast is the only straight-alpha-capable layout CGBitmapContext
    // accepts. Premultiplying black by alpha keeps RGB at 0, so the template-icon
    // self-verification below is unaffected.
    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
) else {
    fputs("error: could not create CGContext\n", stderr)
    exit(1)
}

// CoreGraphics origin is bottom-left; flip so we can think in top-left coords.
ctx.translateBy(x: 0, y: CGFloat(size))
ctx.scaleBy(x: 1, y: -1)
ctx.setShouldAntialias(true)

// Pure black fill; antialiased edges blend black onto transparent,
// which keeps RGB == 0 with reduced alpha — exactly the template contract.
ctx.setFillColor(CGColor(srgbRed: 0, green: 0, blue: 0, alpha: 1))

// --- Glyph (32x32 grid, top-left coords) --------------------------------
// Pennant (small triangle above the masthead, pointing right)
ctx.move(to: CGPoint(x: 10, y: 3))
ctx.addLine(to: CGPoint(x: 19, y: 6.5))
ctx.addLine(to: CGPoint(x: 10, y: 10))
ctx.closePath()
ctx.fillPath()

// Mast (vertical bar)
ctx.fill(CGRect(x: 9, y: 8, width: 3.5, height: 17))

// Sail (bold right-triangle bellying to the right)
ctx.move(to: CGPoint(x: 12.5, y: 10))
ctx.addLine(to: CGPoint(x: 27, y: 17))
ctx.addLine(to: CGPoint(x: 12.5, y: 23.5))
ctx.closePath()
ctx.fillPath()

// Hull (trapezoid: deck line, sloped bow and stern to the keel)
ctx.move(to: CGPoint(x: 4, y: 23.5))
ctx.addLine(to: CGPoint(x: 28, y: 23.5))
ctx.addLine(to: CGPoint(x: 25.5, y: 28))
ctx.addLine(to: CGPoint(x: 6.5, y: 28))
ctx.closePath()
ctx.fillPath()
// ------------------------------------------------------------------------

// --- Self-verification: template-icon contract ---------------------------
var nonTransparent = 0
var nonBlack = 0
var i = 0
while i < pixels.count {
    let alpha = Int(pixels[i + 3])
    if alpha > 0 {
        nonTransparent += 1
        if pixels[i] != 0 || pixels[i + 1] != 0 || pixels[i + 2] != 0 {
            nonBlack += 1
        }
    }
    i += 4
}

if nonTransparent == 0 {
    fputs("error: icon is empty (0 non-transparent pixels)\n", stderr)
    exit(1)
}
if nonBlack > 0 {
    fputs("error: \(nonBlack) non-transparent pixel(s) are not pure black — "
        + "template icon would render with color artifacts\n", stderr)
    exit(1)
}
print("self-verify OK: \(nonTransparent) non-transparent pixels, all pure black (RGB 0,0,0)")

// --- Write PNG -----------------------------------------------------------
guard let image = ctx.makeImage() else {
    fputs("error: could not create CGImage\n", stderr)
    exit(1)
}

// Resolve repo root from this script's location (.omo/scripts/<script>).
let scriptURL = URL(fileURLWithPath: CommandLine.arguments[0]).standardizedFileURL
let repoRoot = scriptURL
    .deletingLastPathComponent() // scripts/
    .deletingLastPathComponent() // .omo/
    .deletingLastPathComponent() // repo root
let outURL = repoRoot.appendingPathComponent("icons/tray-icon.png")

guard let dest = CGImageDestinationCreateWithURL(
    outURL as CFURL, UTType.png.identifier as CFString, 1, nil
) else {
    fputs("error: could not create image destination at \(outURL.path)\n", stderr)
    exit(1)
}
CGImageDestinationAddImage(dest, image, nil)
guard CGImageDestinationFinalize(dest) else {
    fputs("error: could not write PNG to \(outURL.path)\n", stderr)
    exit(1)
}
print("wrote \(outURL.path) (\(size)x\(size))")
