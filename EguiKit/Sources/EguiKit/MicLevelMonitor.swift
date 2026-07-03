@preconcurrency import AVFoundation

/// Microphone permission plus an RMS input-level meter (0...1) sampled from the input node.
@MainActor
final class MicLevelMonitor {
    private let engine = AVAudioEngine()
    private var running = false

    /// Permission result callback, with EGUI permission kind 1 (microphone).
    var onPermission: ((Bool) -> Void)?
    /// Latest level callback in 0...1.
    var onLevel: ((Float) -> Void)?

    func requestPermission() {
        switch AVAudioApplication.shared.recordPermission {
        case .granted:
            onPermission?(true)
        case .undetermined:
            AVAudioApplication.requestRecordPermission { [weak self] granted in
                DispatchQueue.main.async { self?.onPermission?(granted) }
            }
        default:
            onPermission?(false)
        }
    }

    func start() {
        guard !running, AVAudioApplication.shared.recordPermission == .granted else { return }
        let input = engine.inputNode
        let format = input.inputFormat(forBus: 0)
        input.installTap(onBus: 0, bufferSize: 1024, format: format) { [weak self] buffer, _ in
            guard let channel = buffer.floatChannelData?[0] else { return }
            let n = Int(buffer.frameLength)
            guard n > 0 else { return }
            var sum: Float = 0
            for i in 0..<n { let s = channel[i]; sum += s * s }
            let rms = (sum / Float(n)).squareRoot()
            let level = min(1, rms * 20)
            DispatchQueue.main.async { self?.onLevel?(level) }
        }
        do {
            try AVAudioSession.sharedInstance().setCategory(.playAndRecord, options: [.defaultToSpeaker])
            try AVAudioSession.sharedInstance().setActive(true)
            try engine.start()
            running = true
        } catch {
            running = false
        }
    }

    func stop() {
        guard running else { return }
        engine.inputNode.removeTap(onBus: 0)
        engine.stop()
        running = false
    }
}
