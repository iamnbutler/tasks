import SwiftUI

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
    }
}
