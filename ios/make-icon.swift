#!/usr/bin/env swift
// Renders the iPhone app icon: the menubar app's "working" glyph — a green
// half-disc inside a ring — on a dark ground, at 1024 px.
//
//   swift ios/make-icon.swift            # from the repo root
//
// iOS masks icons itself, so the output is a full-bleed opaque square.
import CoreGraphics
import Foundation
import ImageIO
import UniformTypeIdentifiers

let size = 1024
let out = URL(fileURLWithPath: CommandLine.arguments.count > 1
    ? CommandLine.arguments[1]
    : "ios/ClaudeHub/Assets.xcassets/AppIcon.appiconset/AppIcon.png")

let space = CGColorSpace(name: CGColorSpace.sRGB)!
let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8,
                    bytesPerRow: 0, space: space,
                    bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)!
let s = CGFloat(size)

// Ground: the terminal's near-black, a little warmer at the top.
let gradient = CGGradient(colorsSpace: space, colors: [
    CGColor(srgbRed: 0.16, green: 0.155, blue: 0.145, alpha: 1),
    CGColor(srgbRed: 0.07, green: 0.068, blue: 0.064, alpha: 1),
] as CFArray, locations: [0, 1])!
ctx.drawLinearGradient(gradient, start: CGPoint(x: 0, y: s), end: CGPoint(x: 0, y: 0), options: [])

let green = CGColor(srgbRed: 0.345, green: 0.718, blue: 0.478, alpha: 1)
let center = CGPoint(x: s / 2, y: s / 2)
let radius = s * 0.28
ctx.setStrokeColor(green)
ctx.setLineWidth(s * 0.055)
ctx.addArc(center: center, radius: radius, startAngle: 0, endAngle: 2 * .pi, clockwise: false)
ctx.strokePath()
ctx.setFillColor(green)
ctx.move(to: center)
ctx.addArc(center: center, radius: radius, startAngle: .pi / 2, endAngle: 3 * .pi / 2, clockwise: false)
ctx.closePath()
ctx.fillPath()

let image = ctx.makeImage()!
let dest = CGImageDestinationCreateWithURL(out as CFURL, UTType.png.identifier as CFString, 1, nil)!
CGImageDestinationAddImage(dest, image, nil)
guard CGImageDestinationFinalize(dest) else { fatalError("could not write \(out.path)") }
print("wrote \(out.path)")
