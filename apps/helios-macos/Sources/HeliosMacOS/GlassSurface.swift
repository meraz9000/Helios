import SwiftUI

/// Adaptive Liquid Glass styling for Helios on macOS.
///
/// Uses native macOS 26 (Tahoe) `.glassEffect(...)` and `GlassEffectContainer`
/// when compiled with the macOS 26 SDK and running on macOS 26+, and gracefully
/// falls back to a multi-layered specular translucent glass surface on macOS 14–15.
struct HeliosGlassModifier<S: InsettableShape>: ViewModifier {
    let shape: S
    let interactive: Bool
    let tintColor: Color?

    func body(content: Content) -> some View {
        #if compiler(>=6.2)
        if #available(macOS 26.0, *) {
            if let tintColor {
                content.glassEffect(
                    interactive ? .regular.tint(tintColor).interactive() : .regular.tint(tintColor),
                    in: shape
                )
            } else {
                content.glassEffect(
                    interactive ? .regular.interactive() : .regular,
                    in: shape
                )
            }
        } else {
            fallbackGlass(content)
        }
        #else
        fallbackGlass(content)
        #endif
    }

    @ViewBuilder
    private func fallbackGlass(_ content: Content) -> some View {
        content
            .background {
                ZStack {
                    shape.fill(.ultraThinMaterial)
                    if let tintColor {
                        shape.fill(tintColor.opacity(0.18))
                    }
                    // Specular top-edge liquid glass highlight
                    shape.strokeBorder(
                        LinearGradient(
                            colors: [
                                Color.white.opacity(0.38),
                                Color.white.opacity(0.10),
                                Color.white.opacity(0.04),
                                Color.white.opacity(0.16),
                            ],
                            startPoint: .topLeading,
                            endPoint: .bottomTrailing
                        ),
                        lineWidth: 1
                    )
                }
            }
            .shadow(color: Color.black.opacity(0.32), radius: 18, x: 0, y: 10)
    }
}

extension View {
    func heliosGlass<S: InsettableShape>(
        in shape: S,
        interactive: Bool = false,
        tint: Color? = nil
    ) -> some View {
        modifier(HeliosGlassModifier(shape: shape, interactive: interactive, tintColor: tint))
    }

    func heliosGlassCard(cornerRadius: CGFloat = 22, tint: Color? = nil) -> some View {
        heliosGlass(
            in: RoundedRectangle(cornerRadius: cornerRadius, style: .continuous),
            interactive: false,
            tint: tint
        )
    }

    func heliosGlassPill(interactive: Bool = true, tint: Color? = nil) -> some View {
        heliosGlass(in: Capsule(), interactive: interactive, tint: tint)
    }
}

/// Interactive Liquid Glass capsule button with subtle press feedback.
struct HeliosGlassButtonStyle: ButtonStyle {
    var prominent: Bool = false
    var tint: Color = .accentColor

    func makeBody(configuration: Configuration) -> some View {
        configuration.label
            .font(.system(size: 12.5, weight: .semibold))
            .foregroundStyle(prominent ? .white : .primary)
            .padding(.horizontal, 14)
            .padding(.vertical, 7)
            .heliosGlassPill(
                interactive: true,
                tint: prominent ? tint : nil
            )
            .background {
                if prominent {
                    Capsule()
                        .fill(
                            LinearGradient(
                                colors: [tint.opacity(0.88), tint.opacity(0.68)],
                                startPoint: .top,
                                endPoint: .bottom
                            )
                        )
                }
            }
            .scaleEffect(configuration.isPressed ? 0.96 : 1.0)
            .opacity(configuration.isPressed ? 0.88 : 1.0)
            .animation(.spring(response: 0.22, dampingFraction: 0.72), value: configuration.isPressed)
    }
}

/// Compact circular/pill icon button for the floating remote desktop HUD.
struct HeliosHudIconButton: View {
    let systemImage: String
    let helpText: String
    var active: Bool = false
    var tint: Color = .accentColor
    let action: () -> Void

    var body: some View {
        Button(action: action) {
            Image(systemName: systemImage)
                .font(.system(size: 13, weight: .medium))
                .foregroundStyle(active ? .white : .primary)
                .frame(width: 28, height: 28)
                .background {
                    if active {
                        Circle().fill(tint.opacity(0.75))
                    } else {
                        Circle().fill(Color.white.opacity(0.08))
                    }
                }
        }
        .buttonStyle(.plain)
        .help(helpText)
    }
}
