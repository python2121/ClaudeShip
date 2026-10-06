#!/usr/bin/env swift
// Renders the iPhone app icon from web/icon.svg — one drawing for every
// surface — by rasterizing it in a WebKit view at 1024 px. The SVG's
// rounded corners are dropped: iOS masks icons itself, so the output is a
// full-bleed opaque square.
//
//   swift ios/make-icon.swift                      # from the repo root
//   swift ios/make-icon.swift out.png --rounded    # keep the corners (macOS icns source)
import AppKit
import WebKit

let arguments = CommandLine.arguments.dropFirst().filter { !$0.hasPrefix("--") }
let rounded = CommandLine.arguments.contains("--rounded")
let out = URL(fileURLWithPath: arguments.first ?? "ios/ClaudeShip/Assets.xcassets/AppIcon.appiconset/AppIcon.png")
let svgPath = "web/icon.svg"
guard var svg = try? String(contentsOfFile: svgPath, encoding: .utf8) else {
    fatalError("run from the repo root: \(svgPath) not found")
}
if !rounded { svg = svg.replacingOccurrences(of: #"rx="14""#, with: #"rx="0""#) }
let side: CGFloat = 512  // points; the snapshot is scaled to 1024 px below
let html = "<body style='margin:0;background:\(rounded ? "transparent" : "#1a1917")'>"
    + svg.replacingOccurrences(of: "<svg ", with: "<svg width='\(Int(side))' height='\(Int(side))' style='display:block' ")
    + "</body>"

let app = NSApplication.shared
app.setActivationPolicy(.accessory)
let window = NSWindow(contentRect: NSRect(x: 0, y: 0, width: side, height: side),
                      styleMask: [.borderless], backing: .buffered, defer: false)
let web = WKWebView(frame: NSRect(x: 0, y: 0, width: side, height: side))
if rounded { web.setValue(false, forKey: "drawsBackground") }  // transparent outside the corners
window.contentView = web
window.level = NSWindow.Level(rawValue: Int(CGWindowLevelForKey(.desktopWindow)) - 1)
window.orderFrontRegardless()
web.loadHTMLString(html, baseURL: nil)

DispatchQueue.main.asyncAfter(deadline: .now() + 1.5) {
    web.takeSnapshot(with: nil) { image, error in
        guard let image, let cg = image.cgImage(forProposedRect: nil, context: nil, hints: nil) else {
            fatalError("snapshot failed: \(String(describing: error))")
        }
        let size = 1024
        let space = CGColorSpace(name: CGColorSpace.sRGB)!
        let ctx = CGContext(data: nil, width: size, height: size, bitsPerComponent: 8, bytesPerRow: 0,
                            space: space, bitmapInfo: (rounded ? CGImageAlphaInfo.premultipliedLast : .noneSkipLast).rawValue)!
        ctx.interpolationQuality = .high
        ctx.draw(cg, in: CGRect(x: 0, y: 0, width: size, height: size))
        let rep = NSBitmapImageRep(cgImage: ctx.makeImage()!)
        guard let png = rep.representation(using: .png, properties: [:]) else { fatalError("png failed") }
        try! png.write(to: out)
        print("wrote \(out.path) \(size)x\(size)")
        exit(0)
    }
}
app.run()
