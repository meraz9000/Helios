#!/usr/bin/env swift
import AppKit
import CoreGraphics
import Foundation

// Renders a crisp 1024x1024 Helios macOS app iconset using CoreGraphics (zero external deps!)
guard CommandLine.arguments.count > 1 else {
    fputs("Usage: make_icon.swift <output.iconset>\n", stderr)
    exit(1)
}

let iconsetURL = URL(fileURLWithPath: CommandLine.arguments[1])
try FileManager.default.createDirectory(at: iconsetURL, withIntermediateDirectories: true)

func renderIcon(size px: Int) -> Data? {
    let colorSpace = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(
        data: nil,
        width: px,
        height: px,
        bitsPerComponent: 8,
        bytesPerRow: px * 4,
        space: colorSpace,
        bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue
    ) else { return nil }

    let s = CGFloat(px)
    let rect = CGRect(x: 0, y: 0, width: s, height: s)
    let inset = s * 0.08
    let squircleRect = rect.insetBy(dx: inset, dy: inset)
    let cornerRadius = squircleRect.width * 0.225
    let squirclePath = CGPath(roundedRect: squircleRect, cornerWidth: cornerRadius, cornerHeight: cornerRadius, transform: nil)

    // 1. Deep obsidian-indigo liquid glass backdrop
    ctx.saveGState()
    ctx.addPath(squirclePath)
    ctx.clip()

    let bgColors = [
        CGColor(red: 0.09, green: 0.12, blue: 0.22, alpha: 1.0),
        CGColor(red: 0.04, green: 0.06, blue: 0.12, alpha: 1.0),
    ] as CFArray
    if let bgGrad = CGGradient(colorsSpace: colorSpace, colors: bgColors, locations: [0.0, 1.0]) {
        ctx.drawLinearGradient(bgGrad, start: CGPoint(x: 0, y: s), end: CGPoint(x: s, y: 0), options: [])
    }

    // 2. Ambient solar halo
    let center = CGPoint(x: s * 0.5, y: s * 0.5)
    let haloColors = [
        CGColor(red: 1.0, green: 0.62, blue: 0.18, alpha: 0.55),
        CGColor(red: 0.98, green: 0.40, blue: 0.12, alpha: 0.15),
        CGColor(red: 0.98, green: 0.40, blue: 0.12, alpha: 0.0),
    ] as CFArray
    if let haloGrad = CGGradient(colorsSpace: colorSpace, colors: haloColors, locations: [0.0, 0.55, 1.0]) {
        ctx.drawRadialGradient(haloGrad, startCenter: center, startRadius: s * 0.05, endCenter: center, endRadius: s * 0.42, options: [])
    }

    // 3. Translucent frosted glass ring
    let ringRadius = s * 0.27
    ctx.setStrokeColor(CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 0.22))
    ctx.setLineWidth(s * 0.028)
    ctx.addArc(center: center, radius: ringRadius, startAngle: 0, endAngle: .pi * 2, clockwise: false)
    ctx.strokePath()

    // 4. Radiant Helios solar core
    let coreRadius = s * 0.185
    let coreRect = CGRect(x: center.x - coreRadius, y: center.y - coreRadius, width: coreRadius * 2, height: coreRadius * 2)
    ctx.saveGState()
    ctx.addEllipse(in: coreRect)
    ctx.clip()
    let coreColors = [
        CGColor(red: 1.0, green: 0.85, blue: 0.42, alpha: 1.0),
        CGColor(red: 1.0, green: 0.50, blue: 0.14, alpha: 1.0),
        CGColor(red: 0.90, green: 0.26, blue: 0.10, alpha: 1.0),
    ] as CFArray
    if let coreGrad = CGGradient(colorsSpace: colorSpace, colors: coreColors, locations: [0.0, 0.6, 1.0]) {
        ctx.drawLinearGradient(
            coreGrad,
            start: CGPoint(x: center.x - coreRadius, y: center.y + coreRadius),
            end: CGPoint(x: center.x + coreRadius, y: center.y - coreRadius),
            options: []
        )
    }
    ctx.restoreGState()

    // 5. Specular top glass sheen across upper half
    let sheenColors = [
        CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 0.20),
        CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 0.0),
    ] as CFArray
    if let sheenGrad = CGGradient(colorsSpace: colorSpace, colors: sheenColors, locations: [0.0, 1.0]) {
        ctx.drawLinearGradient(sheenGrad, start: CGPoint(x: s * 0.5, y: s * 0.92), end: CGPoint(x: s * 0.5, y: s * 0.48), options: [])
    }

    ctx.restoreGState()

    // 6. Crisp specular rim highlight
    ctx.addPath(squirclePath)
    ctx.setStrokeColor(CGColor(red: 1.0, green: 1.0, blue: 1.0, alpha: 0.28))
    ctx.setLineWidth(max(1.5, s * 0.008))
    ctx.strokePath()

    guard let cgImage = ctx.makeImage() else { return nil }
    let rep = NSBitmapImageRep(cgImage: cgImage)
    return rep.representation(using: .png, properties: [:])
}

let sizes: [(String, Int)] = [
    ("icon_16x16.png", 16),
    ("icon_16x16@2x.png", 32),
    ("icon_32x32.png", 32),
    ("icon_32x32@2x.png", 64),
    ("icon_128x128.png", 128),
    ("icon_128x128@2x.png", 256),
    ("icon_256x256.png", 256),
    ("icon_256x256@2x.png", 512),
    ("icon_512x512.png", 512),
    ("icon_512x512@2x.png", 1024),
]

for (filename, px) in sizes {
    if let png = renderIcon(size: px) {
        try png.write(to: iconsetURL.appendingPathComponent(filename))
    }
}
