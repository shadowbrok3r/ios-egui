import UIKit
import QuartzCore

/// `CAMetalLayer`-backed view that captures touch, keyboard, and trackpad input and forwards it
/// via closures. Transparent so a layer behind it (e.g. camera preview) can show through.
final class MetalHostView: UIView, UIKeyInput {
    override class var layerClass: AnyClass { CAMetalLayer.self }
    var metalLayer: CAMetalLayer { layer as! CAMetalLayer }

    var onTouch: ((UITouch.Phase, CGPoint) -> Void)?
    var onText: ((String) -> Void)?
    var onDeleteBackward: (() -> Void)?
    var onKey: ((Int32, Int32, Bool) -> Void)?
    var onScroll: ((CGFloat, CGFloat) -> Void)?
    var onHover: ((CGPoint) -> Void)?
    var onLayout: (() -> Void)?

    override init(frame: CGRect) {
        super.init(frame: frame)
        metalLayer.isOpaque = true
        backgroundColor = .black
        isMultipleTouchEnabled = true

        let pan = UIPanGestureRecognizer(target: self, action: #selector(handlePan(_:)))
        pan.allowedScrollTypesMask = [.continuous, .discrete]
        pan.maximumNumberOfTouches = 0
        addGestureRecognizer(pan)

        let hover = UIHoverGestureRecognizer(target: self, action: #selector(handleHover(_:)))
        addGestureRecognizer(hover)
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) unavailable") }

    override var canBecomeFirstResponder: Bool { true }

    override func layoutSubviews() {
        super.layoutSubviews()
        let scale = window?.screen.scale ?? UIScreen.main.scale
        metalLayer.contentsScale = scale
        metalLayer.drawableSize = CGSize(
            width: max(1, bounds.width * scale),
            height: max(1, bounds.height * scale)
        )
        onLayout?()
    }

    override func touchesBegan(_ touches: Set<UITouch>, with event: UIEvent?) {
        if let t = touches.first { onTouch?(.began, t.location(in: self)) }
    }
    override func touchesMoved(_ touches: Set<UITouch>, with event: UIEvent?) {
        if let t = touches.first { onTouch?(.moved, t.location(in: self)) }
    }
    override func touchesEnded(_ touches: Set<UITouch>, with event: UIEvent?) {
        if let t = touches.first { onTouch?(.ended, t.location(in: self)) }
    }
    override func touchesCancelled(_ touches: Set<UITouch>, with event: UIEvent?) {
        if let t = touches.first { onTouch?(.cancelled, t.location(in: self)) }
    }

    override func pressesBegan(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        for p in presses where p.key != nil {
            let k = p.key!
            onKey?(Int32(k.keyCode.rawValue), Int32(k.modifierFlags.rawValue), true)
        }
        super.pressesBegan(presses, with: event)
    }
    
    override func pressesEnded(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        for p in presses where p.key != nil {
            let k = p.key!
            onKey?(Int32(k.keyCode.rawValue), Int32(k.modifierFlags.rawValue), false)
        }
        super.pressesEnded(presses, with: event)
    }
    
    override func pressesChanged(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        super.pressesChanged(presses, with: event)
    }
    
    override func pressesCancelled(_ presses: Set<UIPress>, with event: UIPressesEvent?) {
        for p in presses where p.key != nil {
            let k = p.key!
            onKey?(Int32(k.keyCode.rawValue), Int32(k.modifierFlags.rawValue), false)
        }
        super.pressesCancelled(presses, with: event)
    }

    @objc private func handlePan(_ gr: UIPanGestureRecognizer) {
        let t = gr.translation(in: self)
        onScroll?(t.x, t.y)
        gr.setTranslation(.zero, in: self)
    }

    @objc private func handleHover(_ gr: UIHoverGestureRecognizer) {
        onHover?(gr.location(in: self))
    }

    // UIKeyInput (soft keyboard)
    var hasText: Bool { true }
    func insertText(_ text: String) { onText?(text) }
    func deleteBackward() { onDeleteBackward?() }
}
