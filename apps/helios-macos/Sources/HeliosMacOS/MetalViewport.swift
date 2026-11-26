import AppKit
import CoreVideo
import Metal
import MetalKit
import SwiftUI

/// Thread-safe Metal renderer supporting BOTH:
/// 1. Live RGBA dirty-tile `MTLTexture` updates (Survival / Text Mode)
/// 2. Zero-copy `CVMetalTextureCache` `CVPixelBuffer` rendering from Apple `VideoToolbox`
final class HeliosMetalRenderer: NSObject, MTKViewDelegate {
    private let device: MTLDevice
    private let commandQueue: MTLCommandQueue
    private let pipelineState: MTLRenderPipelineState
    private let samplerState: MTLSamplerState
    private var textureCache: CVMetalTextureCache?

    private let lock = NSLock()
    private var rgbaTexture: MTLTexture?
    private var cvMetalTexture: CVMetalTexture?
    private var activeTexture: MTLTexture?
    private(set) var frameWidth: Int = 0
    private(set) var frameHeight: Int = 0

    private static let shaderSource = """
    #include <metal_stdlib>
    using namespace metal;

    struct VertexOut {
        float4 position [[position]];
        float2 texCoord;
    };

    struct ViewportUniforms {
        float2 scale;
    };

    vertex VertexOut heliosVertex(
        uint vid [[vertex_id]],
        constant ViewportUniforms &u [[buffer(0)]]
    ) {
        const float2 pos[4] = {
            float2(-1.0, -1.0),
            float2( 1.0, -1.0),
            float2(-1.0,  1.0),
            float2( 1.0,  1.0)
        };
        const float2 uv[4] = {
            float2(0.0, 1.0),
            float2(1.0, 1.0),
            float2(0.0, 0.0),
            float2(1.0, 0.0)
        };
        VertexOut out;
        out.position = float4(pos[vid] * u.scale, 0.0, 1.0);
        out.texCoord = uv[vid];
        return out;
    }

    fragment float4 heliosFragment(
        VertexOut in [[stage_in]],
        texture2d<float> tex [[texture(0)]],
        sampler s [[sampler(0)]]
    ) {
        return tex.sample(s, in.texCoord);
    }
    """

    init?(device: MTLDevice) {
        self.device = device
        guard let queue = device.makeCommandQueue() else { return nil }
        self.commandQueue = queue

        do {
            let library = try device.makeLibrary(source: Self.shaderSource, options: nil)
            let desc = MTLRenderPipelineDescriptor()
            desc.vertexFunction = library.makeFunction(name: "heliosVertex")
            desc.fragmentFunction = library.makeFunction(name: "heliosFragment")
            desc.colorAttachments[0].pixelFormat = .bgra8Unorm
            self.pipelineState = try device.makeRenderPipelineState(descriptor: desc)
        } catch {
            NSLog("HeliosMetalRenderer shader compile error: \(error)")
            return nil
        }

        let sampDesc = MTLSamplerDescriptor()
        sampDesc.minFilter = .linear
        sampDesc.magFilter = .linear
        sampDesc.sAddressMode = .clampToEdge
        sampDesc.tAddressMode = .clampToEdge
        guard let samp = device.makeSamplerState(descriptor: sampDesc) else { return nil }
        self.samplerState = samp

        var cache: CVMetalTextureCache?
        CVMetalTextureCacheCreate(kCFAllocatorDefault, nil, device, nil, &cache)
        self.textureCache = cache

        super.init()
    }

    /// Upload an RGBA8888 framebuffer produced by Survival / Text Mode dirty-tile decoding.
    func updateRGBA(width: Int, height: Int, bytes: UnsafePointer<UInt8>) {
        guard width > 0, height > 0 else { return }
        lock.lock()
        defer { lock.unlock() }

        if rgbaTexture == nil || rgbaTexture?.width != width || rgbaTexture?.height != height {
            let desc = MTLTextureDescriptor.texture2DDescriptor(
                pixelFormat: .rgba8Unorm,
                width: width,
                height: height,
                mipmapped: false
            )
            desc.usage = [.shaderRead]
            rgbaTexture = device.makeTexture(descriptor: desc)
        }

        if let tex = rgbaTexture {
            tex.replace(
                region: MTLRegionMake2D(0, 0, width, height),
                mipmapLevel: 0,
                withBytes: bytes,
                bytesPerRow: width * 4
            )
            activeTexture = tex
            cvMetalTexture = nil
            frameWidth = width
            frameHeight = height
        }
    }

    /// Bind a hardware-decoded `CVPixelBuffer` (`BGRA32`) from Apple `VideoToolbox` with zero CPU copy.
    func updatePixelBuffer(_ pixelBuffer: CVPixelBuffer) {
        guard let cache = textureCache else { return }
        let width = CVPixelBufferGetWidth(pixelBuffer)
        let height = CVPixelBufferGetHeight(pixelBuffer)
        guard width > 0, height > 0 else { return }

        var cvTex: CVMetalTexture?
        let status = CVMetalTextureCacheCreateTextureFromImage(
            kCFAllocatorDefault,
            cache,
            pixelBuffer,
            nil,
            .bgra8Unorm,
            width,
            height,
            0,
            &cvTex
        )
        guard status == kCVReturnSuccess, let cvTex, let mtlTex = CVMetalTextureGetTexture(cvTex) else {
            return
        }

        lock.lock()
        cvMetalTexture = cvTex
        activeTexture = mtlTex
        frameWidth = width
        frameHeight = height
        lock.unlock()
    }

    func mtkView(_ view: MTKView, drawableSizeWillChange size: CGSize) {}

    func draw(in view: MTKView) {
        guard let passDesc = view.currentRenderPassDescriptor,
              let drawable = view.currentDrawable,
              let cmdBuf = commandQueue.makeCommandBuffer(),
              let encoder = cmdBuf.makeRenderCommandEncoder(descriptor: passDesc)
        else { return }

        lock.lock()
        let tex = activeTexture
        let fw = frameWidth
        let fh = frameHeight
        lock.unlock()

        if let tex, fw > 0, fh > 0 {
            let viewW = Float(max(view.drawableSize.width, 1))
            let viewH = Float(max(view.drawableSize.height, 1))
            let frameAspect = Float(fw) / Float(fh)
            let viewAspect = viewW / viewH

            var scale = SIMD2<Float>(1.0, 1.0)
            if frameAspect > viewAspect {
                scale.y = viewAspect / frameAspect
            } else {
                scale.x = frameAspect / viewAspect
            }

            encoder.setRenderPipelineState(pipelineState)
            encoder.setVertexBytes(&scale, length: MemoryLayout<SIMD2<Float>>.size, index: 0)
            encoder.setFragmentTexture(tex, index: 0)
            encoder.setFragmentSamplerState(samplerState, index: 0)
            encoder.drawPrimitives(type: .triangleStrip, vertexStart: 0, vertexCount: 4)
        }

        encoder.endEncoding()
        cmdBuf.present(drawable)
        cmdBuf.commit()
    }

    /// Convert a point inside `viewBounds` to normalized `[0, 1]` remote desktop coordinates,
    /// accounting for aspect-ratio letterboxing / pillarboxing.
    func normalizedRemotePoint(from viewPoint: NSPoint, in viewBounds: NSRect) -> (Float, Float)? {
        lock.lock()
        let fw = frameWidth
        let fh = frameHeight
        lock.unlock()

        let viewW = max(viewBounds.width, 1)
        let viewH = max(viewBounds.height, 1)

        var contentRect = viewBounds
        if fw > 0, fh > 0 {
            let frameAspect = CGFloat(fw) / CGFloat(fh)
            let viewAspect = viewW / viewH
            if frameAspect > viewAspect {
                let h = viewW / frameAspect
                contentRect = NSRect(x: 0, y: (viewH - h) / 2, width: viewW, height: h)
            } else {
                let w = viewH * frameAspect
                contentRect = NSRect(x: (viewW - w) / 2, y: 0, width: w, height: viewH)
            }
        }

        let nx = Float((viewPoint.x - contentRect.minX) / max(contentRect.width, 1))
        // Flip Y (AppKit origin is bottom-left, Windows screen origin is top-left)
        let ny = Float(1.0 - (viewPoint.y - contentRect.minY) / max(contentRect.height, 1))
        return (min(max(nx, 0.0), 1.0), min(max(ny, 0.0), 1.0))
    }
}

/// Interactive `MTKView` capturing native macOS mouse, scroll, and keyboard events
/// and rendering the Windows Host's cursor shape natively at local refresh rate.
final class HeliosInteractiveMetalView: MTKView {
    var rendererRef: HeliosMetalRenderer?
    var mapCommandToCtrl: Bool = true
    var remoteCursor: NSCursor? {
        didSet {
            window?.invalidateCursorRects(for: self)
        }
    }
    var onMouseMoveAbs: ((Float, Float) -> Void)?
    var onMouseButton: ((UInt8, Bool) -> Void)?
    var onMouseWheel: ((Int32, Int32) -> Void)?
    var onKey: ((UInt16, Bool) -> Void)?

    private var trackingArea: NSTrackingArea?
    private var prevModifiers: NSEvent.ModifierFlags = []

    override var acceptsFirstResponder: Bool { true }

    override func resetCursorRects() {
        super.resetCursorRects()
        if let cursor = remoteCursor {
            addCursorRect(bounds, cursor: cursor)
        }
    }

    override func updateTrackingAreas() {
        super.updateTrackingAreas()
        if let existing = trackingArea {
            removeTrackingArea(existing)
        }
        let area = NSTrackingArea(
            rect: bounds,
            options: [.activeInKeyWindow, .mouseMoved, .inVisibleRect],
            owner: self,
            userInfo: nil
        )
        addTrackingArea(area)
        trackingArea = area
    }

    private func forwardPointer(_ event: NSEvent) {
        let local = convert(event.locationInWindow, from: nil)
        if let (nx, ny) = rendererRef?.normalizedRemotePoint(from: local, in: bounds) {
            onMouseMoveAbs?(nx, ny)
        }
    }

    override func mouseMoved(with event: NSEvent) { forwardPointer(event) }
    override func mouseDragged(with event: NSEvent) { forwardPointer(event) }
    override func rightMouseDragged(with event: NSEvent) { forwardPointer(event) }
    override func otherMouseDragged(with event: NSEvent) { forwardPointer(event) }

    override func mouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        forwardPointer(event)
        onMouseButton?(0, true)
    }

    override func mouseUp(with event: NSEvent) {
        forwardPointer(event)
        onMouseButton?(0, false)
    }

    override func rightMouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        forwardPointer(event)
        onMouseButton?(1, true)
    }

    override func rightMouseUp(with event: NSEvent) {
        forwardPointer(event)
        onMouseButton?(1, false)
    }

    override func otherMouseDown(with event: NSEvent) {
        window?.makeFirstResponder(self)
        forwardPointer(event)
        onMouseButton?(2, true)
    }

    override func otherMouseUp(with event: NSEvent) {
        forwardPointer(event)
        onMouseButton?(2, false)
    }

    override func scrollWheel(with event: NSEvent) {
        let dx = Int32(event.scrollingDeltaX.rounded())
        let dy = Int32(event.scrollingDeltaY.rounded())
        if dx != 0 || dy != 0 {
            onMouseWheel?(dx, dy)
        }
    }

    override func keyDown(with event: NSEvent) {
        if let vk = MacKeyMapper.macKeyCodeToWindowsVK(event.keyCode, mapCommandToCtrl: mapCommandToCtrl) {
            onKey?(vk, true)
        }
    }

    override func keyUp(with event: NSEvent) {
        if let vk = MacKeyMapper.macKeyCodeToWindowsVK(event.keyCode, mapCommandToCtrl: mapCommandToCtrl) {
            onKey?(vk, false)
        }
    }

    override func flagsChanged(with event: NSEvent) {
        let curr = event.modifierFlags
        let pairs: [(NSEvent.ModifierFlags, UInt16)] = [
            (.shift, MacKeyMapper.vkShift),
            (.control, MacKeyMapper.vkControl),
            (.option, MacKeyMapper.vkMenuAlt),
            (.command, mapCommandToCtrl ? MacKeyMapper.vkControl : MacKeyMapper.vkLWin),
        ]
        for (flag, vk) in pairs {
            let wasDown = prevModifiers.contains(flag)
            let isDown = curr.contains(flag)
            if wasDown != isDown {
                onKey?(vk, isDown)
            }
        }
        prevModifiers = curr
    }
}

/// SwiftUI wrapper around `HeliosInteractiveMetalView`.
struct MetalViewportRepresentable: NSViewRepresentable {
    let renderer: HeliosMetalRenderer?
    let remoteCursor: NSCursor?
    let mapCommandToCtrl: Bool
    let onMouseMoveAbs: (Float, Float) -> Void
    let onMouseButton: (UInt8, Bool) -> Void
    let onMouseWheel: (Int32, Int32) -> Void
    let onKey: (UInt16, Bool) -> Void

    func makeNSView(context: Context) -> HeliosInteractiveMetalView {
        let device = MTLCreateSystemDefaultDevice()
        let view = HeliosInteractiveMetalView(frame: .zero, device: device)
        view.colorPixelFormat = .bgra8Unorm
        view.clearColor = MTLClearColor(red: 0.086, green: 0.106, blue: 0.133, alpha: 1.0)
        view.preferredFramesPerSecond = 60
        view.enableSetNeedsDisplay = false
        view.isPaused = false
        view.delegate = renderer
        view.rendererRef = renderer
        return view
    }

    func updateNSView(_ view: HeliosInteractiveMetalView, context: Context) {
        view.remoteCursor = remoteCursor
        view.mapCommandToCtrl = mapCommandToCtrl
        view.onMouseMoveAbs = onMouseMoveAbs
        view.onMouseButton = onMouseButton
        view.onMouseWheel = onMouseWheel
        view.onKey = onKey
    }
}
