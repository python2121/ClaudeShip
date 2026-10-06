import SwiftUI

/// Drop-in replacement for SwiftUI's `@State` that builds with the bare
/// Command Line Tools.
///
/// Starting with the macOS 27 SDK, `@State` is a Swift macro
/// (`#externalMacro(module: "SwiftUIMacros", type: "StateMacro")`) whose
/// plugin, `libSwiftUIMacros.dylib`, ships only inside Xcode. The Command
/// Line Tools carry the SDK but not the plugin, so any `@State` in this
/// package fails with "plugin for module 'SwiftUIMacros' not found" and a
/// cascade of "'self' is immutable" errors.
///
/// The underlying `State<Value>` struct is still in the SDK, so this wrapper
/// stores one and forwards to it. Swift's ordinary property-wrapper
/// synthesis (no macro needed) supplies `$binding`, and SwiftUI installs the
/// nested `State` because this type conforms to `DynamicProperty`. Behavior
/// matches the classic (pre-macro) `@State`: the initial value is evaluated
/// each time the view struct is re-created, and the first one wins.
///
/// Use `@ViewState` everywhere you would have written `@State`. The build
/// script rejects `@State` so this can't silently regress on a machine that
/// happens to have Xcode.
@propertyWrapper
struct ViewState<Value>: DynamicProperty {
    private var storage: State<Value>

    init(wrappedValue: Value) {
        storage = State(initialValue: wrappedValue)
    }

    var wrappedValue: Value {
        get { storage.wrappedValue }
        nonmutating set { storage.wrappedValue = newValue }
    }

    var projectedValue: Binding<Value> { storage.projectedValue }
}
