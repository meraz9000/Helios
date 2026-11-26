import CoreMedia
import CoreVideo
import Foundation
import VideoToolbox

/// Hardware-accelerated Apple VideoToolbox H.264 / HEVC Annex-B decoder.
///
/// Parses Annex-B NAL units (`0x00 0x00 0x01` / `0x00 0x00 0x00 0x01`), extracts
/// SPS/PPS (or VPS/SPS/PPS for HEVC) to construct `CMVideoFormatDescription`,
/// converts NALUs into AVCC 4-byte length-prefixed `CMSampleBuffer`s, and invokes
/// `onDecodedPixelBuffer` with a zero-copy `CVPixelBuffer` (`BGRA32`) ready for
/// `CVMetalTextureCache`.
final class VideoToolboxDecoder {
    private var formatDesc: CMVideoFormatDescription?
    private var session: VTDecompressionSession?
    private var sps: Data?
    private var pps: Data?
    private var vps: Data?
    private var activeCodec: UInt8 = 0 // 0 = H.264, 1 = HEVC

    var onDecodedPixelBuffer: ((CVPixelBuffer) -> Void)?

    deinit {
        invalidate()
    }

    func invalidate() {
        if let s = session {
            VTDecompressionSessionInvalidate(s)
            session = nil
        }
        formatDesc = nil
        sps = nil
        pps = nil
        vps = nil
    }

    /// Feed one Annex-B encoded video access unit (`codec`: 0 = H.264, 1 = H.265/HEVC).
    func decodeAnnexB(_ annexB: Data, codec: UInt8, timestampUs: UInt64) {
        if codec != activeCodec {
            invalidate()
            activeCodec = codec
        }

        let nalUnits = splitAnnexBNALUs(annexB)
        if nalUnits.isEmpty { return }

        var avccData = Data()
        var paramsUpdated = false

        for nalu in nalUnits {
            guard let firstByte = nalu.first else { continue }

            if codec == 0 {
                // H.264 NAL unit type (lower 5 bits)
                let nalType = firstByte & 0x1F
                if nalType == 7 {
                    if sps != nalu {
                        sps = nalu
                        paramsUpdated = true
                    }
                    continue
                } else if nalType == 8 {
                    if pps != nalu {
                        pps = nalu
                        paramsUpdated = true
                    }
                    continue
                } else if nalType == 6 || nalType == 9 {
                    // SEI / AUD
                    continue
                }
            } else if codec == 1 {
                // HEVC NAL unit type (bits 1..6)
                let nalType = (firstByte >> 1) & 0x3F
                if nalType == 32 {
                    if vps != nalu {
                        vps = nalu
                        paramsUpdated = true
                    }
                    continue
                } else if nalType == 33 {
                    if sps != nalu {
                        sps = nalu
                        paramsUpdated = true
                    }
                    continue
                } else if nalType == 34 {
                    if pps != nalu {
                        pps = nalu
                        paramsUpdated = true
                    }
                    continue
                }
            }

            var bigLen = UInt32(nalu.count).bigEndian
            withUnsafeBytes(of: &bigLen) { avccData.append(contentsOf: $0) }
            avccData.append(nalu)
        }

        if paramsUpdated || session == nil {
            ensureSession(codec: codec)
        }

        guard let session, let formatDesc, !avccData.isEmpty else { return }

        var blockBuffer: CMBlockBuffer?
        let status = avccData.withUnsafeBytes { rawBuf -> OSStatus in
            guard let base = rawBuf.baseAddress else { return -1 }
            var buf: CMBlockBuffer?
            let st1 = CMBlockBufferCreateWithMemoryBlock(
                allocator: kCFAllocatorDefault,
                memoryBlock: nil,
                blockLength: avccData.count,
                blockAllocator: kCFAllocatorDefault,
                customBlockSource: nil,
                offsetToData: 0,
                dataLength: avccData.count,
                flags: 0,
                blockBufferOut: &buf
            )
            guard st1 == kCMBlockBufferNoErr, let created = buf else { return st1 }
            let st2 = CMBlockBufferReplaceDataBytes(
                with: base,
                blockBuffer: created,
                offsetIntoDestination: 0,
                dataLength: avccData.count
            )
            blockBuffer = created
            return st2
        }

        guard status == kCMBlockBufferNoErr, let blockBuffer else { return }

        var sampleBuffer: CMSampleBuffer?
        var timing = CMSampleTimingInfo(
            duration: .invalid,
            presentationTimeStamp: CMTime(value: Int64(timestampUs), timescale: 1_000_000),
            decodeTimeStamp: .invalid
        )
        var sampleSize = avccData.count

        let sbStatus = CMSampleBufferCreateReady(
            allocator: kCFAllocatorDefault,
            dataBuffer: blockBuffer,
            formatDescription: formatDesc,
            sampleCount: 1,
            sampleTimingEntryCount: 1,
            sampleTimingArray: &timing,
            sampleSizeEntryCount: 1,
            sampleSizeArray: &sampleSize,
            sampleBufferOut: &sampleBuffer
        )

        guard sbStatus == noErr, let sampleBuffer else { return }

        var flagsOut = VTDecodeInfoFlags()
        let decodeFlags: VTDecodeFrameFlags = [._EnableAsynchronousDecompression]
        VTDecompressionSessionDecodeFrame(
            session,
            sampleBuffer: sampleBuffer,
            flags: decodeFlags,
            infoFlagsOut: &flagsOut
        ) { [weak self] status, _, imageBuffer, _, _ in
            guard status == noErr, let pixelBuffer = imageBuffer else { return }
            self?.onDecodedPixelBuffer?(pixelBuffer)
        }
    }

    private func ensureSession(codec: UInt8) {
        var newDesc: CMVideoFormatDescription?

        if codec == 0 {
            guard let sps, let pps else { return }
            let status: OSStatus = sps.withUnsafeBytes { spsBuf in
                pps.withUnsafeBytes { ppsBuf in
                    let ptrs = [
                        spsBuf.bindMemory(to: UInt8.self).baseAddress!,
                        ppsBuf.bindMemory(to: UInt8.self).baseAddress!,
                    ]
                    let sizes = [sps.count, pps.count]
                    return CMVideoFormatDescriptionCreateFromH264ParameterSets(
                        allocator: kCFAllocatorDefault,
                        parameterSetCount: 2,
                        parameterSetPointers: ptrs,
                        parameterSetSizes: sizes,
                        nalUnitHeaderLength: 4,
                        formatDescriptionOut: &newDesc
                    )
                }
            }
            guard status == noErr else { return }
        } else if codec == 1 {
            guard let vps, let sps, let pps else { return }
            let status: OSStatus = vps.withUnsafeBytes { vpsBuf in
                sps.withUnsafeBytes { spsBuf in
                    pps.withUnsafeBytes { ppsBuf in
                        let ptrs = [
                            vpsBuf.bindMemory(to: UInt8.self).baseAddress!,
                            spsBuf.bindMemory(to: UInt8.self).baseAddress!,
                            ppsBuf.bindMemory(to: UInt8.self).baseAddress!,
                        ]
                        let sizes = [vps.count, sps.count, pps.count]
                        return CMVideoFormatDescriptionCreateFromHEVCParameterSets(
                            allocator: kCFAllocatorDefault,
                            parameterSetCount: 3,
                            parameterSetPointers: ptrs,
                            parameterSetSizes: sizes,
                            nalUnitHeaderLength: 4,
                            extensions: nil,
                            formatDescriptionOut: &newDesc
                        )
                    }
                }
            }
            guard status == noErr else { return }
        }

        guard let desc = newDesc else { return }
        if let old = session {
            VTDecompressionSessionInvalidate(old)
            session = nil
        }
        formatDesc = desc

        let destAttrs: [CFString: Any] = [
            kCVPixelBufferPixelFormatTypeKey: kCVPixelFormatType_32BGRA,
            kCVPixelBufferMetalCompatibilityKey: true,
        ]
        let decoderSpec: [CFString: Any] = [
            kVTVideoDecoderSpecification_EnableHardwareAcceleratedVideoDecoder: true,
        ]

        var newSession: VTDecompressionSession?
        let st = VTDecompressionSessionCreate(
            allocator: kCFAllocatorDefault,
            formatDescription: desc,
            decoderSpecification: decoderSpec as CFDictionary,
            imageBufferAttributes: destAttrs as CFDictionary,
            outputCallback: nil,
            decompressionSessionOut: &newSession
        )
        if st == noErr, let created = newSession {
            VTSessionSetProperty(
                created,
                key: kVTDecompressionPropertyKey_RealTime,
                value: kCFBooleanTrue
            )
            session = created
        }
    }

    private func splitAnnexBNALUs(_ data: Data) -> [Data] {
        var nalus: [Data] = []
        let bytes = [UInt8](data)
        let count = bytes.count
        var i = 0
        var nalStart: Int?

        while i < count {
            var startCodeLen = 0
            if i + 3 < count && bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 0 && bytes[i + 3] == 1 {
                startCodeLen = 4
            } else if i + 2 < count && bytes[i] == 0 && bytes[i + 1] == 0 && bytes[i + 2] == 1 {
                startCodeLen = 3
            }

            if startCodeLen > 0 {
                if let start = nalStart, i > start {
                    nalus.append(Data(bytes[start..<i]))
                }
                i += startCodeLen
                nalStart = i
            } else {
                i += 1
            }
        }

        if let start = nalStart, start < count {
            nalus.append(Data(bytes[start..<count]))
        }
        return nalus
    }
}
