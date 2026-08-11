import SwiftUI

/// Version identity, stamped by `make app`: MARKETING_VERSION becomes
/// `0.1.<commit count>` and CURRENT_PROJECT_VERSION the short git SHA
/// (`-dirty` when the tree had uncommitted changes). An Xcode-launched build
/// shows the static project values instead — if About says `0.1 (1)`, you're
/// not looking at a `make app` install.
enum BuildInfo {
    static var version: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleShortVersionString") as? String ?? "dev"
    }
    static var commit: String {
        Bundle.main.object(forInfoDictionaryKey: "CFBundleVersion") as? String ?? "?"
    }
}

@main
struct TasksApp: App {
    @State private var model = AppModel()

    var body: some Scene {
        WindowGroup {
            ContentView()
                .environment(model)
                .task { await model.start() }
                .frame(minWidth: 480, minHeight: 360)
        }
        .defaultSize(width: 640, height: 720)
        .commands {
            CommandGroup(replacing: .appInfo) {
                Button("About Tasks") {
                    NSApplication.shared.orderFrontStandardAboutPanel(options: [
                        .credits: NSAttributedString(
                            string: "commit \(BuildInfo.commit)",
                            attributes: [
                                .font: NSFont.monospacedSystemFont(
                                    ofSize: NSFont.smallSystemFontSize, weight: .regular),
                                .foregroundColor: NSColor.secondaryLabelColor,
                            ])
                    ])
                }
            }
        }
    }
}
