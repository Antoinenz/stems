// The app's two surfaces, both opaque: the main content and the panes
// beside it. Every column paints one of them all the way up under the
// toolbar, so nothing shows through and no pane looks different from its
// neighbour.

import AppKit
import SwiftUI

enum Surface {
    /// Tables, the player, cards: the main content.
    static let content = Color(nsColor: .textBackgroundColor)
    /// Panes beside it: the library browser, the song panel, the queue,
    /// the sidebar.
    static let pane = Color(nsColor: .windowBackgroundColor)
    /// Cards and boxes sitting on either: a step lighter (dark) or darker
    /// (light), still opaque because what's under it is.
    static let raised = Color.primary.opacity(0.06)
}

extension View {
    /// Paint `color` behind this column, up under the toolbar too.
    func surface(_ color: Color) -> some View {
        background { color.ignoresSafeArea(edges: .top) }
    }
}
