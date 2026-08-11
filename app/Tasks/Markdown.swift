import SwiftUI

/// Dependency-free block-level markdown renderer. `AttributedString`'s
/// markdown init only handles inline styling, which left the orchestrator's
/// tables, code fences, and headings rendering as raw text — this covers the
/// block shapes GitHub-flavored markdown actually produces in this app
/// (issue bodies, spec content, orchestrator replies). Inline styling within
/// blocks still goes through `AttributedString`.
struct MarkdownView: View {
    let text: String

    var body: some View {
        // Cached: SwiftUI re-evaluates body on every scroll frame of a lazy
        // container (and on every streaming delta), and parsing — especially
        // AttributedString's markdown init — is far too expensive for that.
        let blocks = MarkdownCache.blocks(for: text)
        VStack(alignment: .leading, spacing: 8) {
            ForEach(Array(blocks.enumerated()), id: \.offset) { _, block in
                BlockView(block: block)
            }
        }
        .frame(maxWidth: .infinity, alignment: .leading)
        // Deliberately NOT .textSelection(.enabled) here: container-wide
        // selection turns every table cell and list row into its own AppKit
        // selectable-text host — hundreds per chat — which is what locked
        // scrolling up. BlockView opts in per copy-worthy block instead.
    }
}

/// Memoizes the two expensive steps behind rendering: block parsing and
/// inline `AttributedString(markdown:)`. Keyed by source text — chat
/// messages, issue bodies, and specs are immutable once written, and the one
/// mutating case (the streaming live reply) changes the key each delta, so a
/// stale entry is never served. NSCache bounds memory; main-actor isolation
/// satisfies strict concurrency (all callers are SwiftUI view bodies).
@MainActor
enum MarkdownCache {
    private final class Box<T> {
        let value: T
        init(_ value: T) { self.value = value }
    }

    private static let parsed: NSCache<NSString, AnyObject> = {
        let cache = NSCache<NSString, AnyObject>()
        cache.countLimit = 512
        return cache
    }()
    private static let inlined: NSCache<NSString, AnyObject> = {
        let cache = NSCache<NSString, AnyObject>()
        cache.countLimit = 4096
        return cache
    }()

    static func blocks(for text: String) -> [MarkdownBlock] {
        let key = text as NSString
        if let hit = parsed.object(forKey: key) as? Box<[MarkdownBlock]> {
            return hit.value
        }
        let blocks = MarkdownParser.parse(text)
        parsed.setObject(Box(blocks), forKey: key)
        return blocks
    }

    static func inline(_ text: String) -> AttributedString {
        let key = text as NSString
        if let hit = inlined.object(forKey: key) as? Box<AttributedString> {
            return hit.value
        }
        let attributed =
            (try? AttributedString(
                markdown: text,
                options: .init(interpretedSyntax: .inlineOnlyPreservingWhitespace)))
            ?? AttributedString(text)
        inlined.setObject(Box(attributed), forKey: key)
        return attributed
    }
}

enum MarkdownBlock {
    case heading(level: Int, text: String)
    case paragraph(String)
    case code(String)
    case list(items: [MarkdownListItem])
    case table(header: [String], rows: [[String]])
    case quote(String)
    case rule
}

struct MarkdownListItem {
    /// Nesting depth from leading indentation.
    let indent: Int
    /// The source marker: "•" for bullets, "3." for ordered items.
    let marker: String
    let text: String
}

private struct BlockView: View {
    let block: MarkdownBlock

    var body: some View {
        switch block {
        case .heading(let level, let text):
            inline(text)
                .font(headingFont(level))
                .padding(.top, level <= 2 ? 4 : 2)
        case .paragraph(let text):
            inline(text)
                .textSelection(.enabled)
        case .code(let code):
            ScrollView(.horizontal, showsIndicators: false) {
                Text(code)
                    .font(.callout.monospaced())
                    .textSelection(.enabled)
                    .padding(8)
            }
            .background(.quaternary.opacity(0.5), in: RoundedRectangle(cornerRadius: 6))
        case .list(let items):
            VStack(alignment: .leading, spacing: 3) {
                ForEach(Array(items.enumerated()), id: \.offset) { _, item in
                    HStack(alignment: .firstTextBaseline, spacing: 6) {
                        Text(item.marker)
                            .foregroundStyle(.secondary)
                            .monospacedDigit()
                        inline(item.text)
                    }
                    .padding(.leading, CGFloat(item.indent) * 16)
                }
            }
        case .table(let header, let rows):
            ScrollView(.horizontal, showsIndicators: false) {
                Grid(alignment: .leadingFirstTextBaseline, horizontalSpacing: 14, verticalSpacing: 5) {
                    GridRow {
                        ForEach(Array(header.enumerated()), id: \.offset) { _, cell in
                            inline(cell).bold()
                        }
                    }
                    Divider()
                    ForEach(Array(rows.enumerated()), id: \.offset) { _, row in
                        GridRow {
                            ForEach(Array(row.enumerated()), id: \.offset) { _, cell in
                                inline(cell)
                            }
                        }
                    }
                }
                .padding(.vertical, 2)
            }
        case .quote(let text):
            HStack(alignment: .top, spacing: 8) {
                RoundedRectangle(cornerRadius: 1.5)
                    .fill(.tertiary)
                    .frame(width: 3)
                inline(text)
                    .foregroundStyle(.secondary)
            }
            .fixedSize(horizontal: false, vertical: true)
        case .rule:
            Divider()
        }
    }

    private func headingFont(_ level: Int) -> Font {
        switch level {
        case 1: .title2.weight(.semibold)
        case 2: .title3.weight(.semibold)
        case 3: .headline
        default: .subheadline.weight(.semibold)
        }
    }

    private func inline(_ text: String) -> Text {
        Text(MarkdownCache.inline(text))
    }
}

enum MarkdownParser {
    static func parse(_ text: String) -> [MarkdownBlock] {
        let lines = text
            .replacingOccurrences(of: "\r\n", with: "\n")
            .components(separatedBy: "\n")
        var blocks: [MarkdownBlock] = []
        var paragraph: [String] = []

        func flushParagraph() {
            if !paragraph.isEmpty {
                blocks.append(.paragraph(paragraph.joined(separator: "\n")))
                paragraph = []
            }
        }

        var i = 0
        while i < lines.count {
            let line = lines[i]
            let trimmed = line.trimmingCharacters(in: .whitespaces)

            if trimmed.hasPrefix("```") {
                flushParagraph()
                var code: [String] = []
                i += 1
                while i < lines.count,
                    !lines[i].trimmingCharacters(in: .whitespaces).hasPrefix("```")
                {
                    code.append(lines[i])
                    i += 1
                }
                i += 1  // closing fence (or EOF)
                blocks.append(.code(code.joined(separator: "\n")))
                continue
            }
            if trimmed.isEmpty {
                flushParagraph()
                i += 1
                continue
            }
            if let (level, rest) = heading(trimmed) {
                flushParagraph()
                blocks.append(.heading(level: level, text: rest))
                i += 1
                continue
            }
            if isRule(trimmed) {
                flushParagraph()
                blocks.append(.rule)
                i += 1
                continue
            }
            if trimmed.hasPrefix("|"), i + 1 < lines.count,
                isTableSeparator(lines[i + 1].trimmingCharacters(in: .whitespaces))
            {
                flushParagraph()
                let header = cells(of: trimmed)
                var rows: [[String]] = []
                i += 2
                while i < lines.count {
                    let rowLine = lines[i].trimmingCharacters(in: .whitespaces)
                    guard rowLine.hasPrefix("|") else { break }
                    rows.append(cells(of: rowLine))
                    i += 1
                }
                // Ragged rows render as empty cells rather than crashing Grid.
                let width = ([header] + rows).map(\.count).max() ?? 0
                let padded = rows.map { $0 + Array(repeating: "", count: width - $0.count) }
                blocks.append(
                    .table(
                        header: header + Array(repeating: "", count: width - header.count),
                        rows: padded))
                continue
            }
            if listItem(line) != nil {
                flushParagraph()
                var items: [MarkdownListItem] = []
                while i < lines.count, let item = listItem(lines[i]) {
                    items.append(item)
                    i += 1
                }
                blocks.append(.list(items: items))
                continue
            }
            if trimmed.hasPrefix(">") {
                flushParagraph()
                var quoted: [String] = []
                while i < lines.count {
                    let q = lines[i].trimmingCharacters(in: .whitespaces)
                    guard q.hasPrefix(">") else { break }
                    quoted.append(String(q.dropFirst()).trimmingCharacters(in: .whitespaces))
                    i += 1
                }
                blocks.append(.quote(quoted.joined(separator: "\n")))
                continue
            }
            paragraph.append(line)
            i += 1
        }
        flushParagraph()
        return blocks
    }

    private static func heading(_ line: String) -> (Int, String)? {
        let hashes = line.prefix(while: { $0 == "#" })
        guard (1...6).contains(hashes.count) else { return nil }
        let rest = line.dropFirst(hashes.count)
        guard rest.first == " " else { return nil }
        return (hashes.count, rest.trimmingCharacters(in: .whitespaces))
    }

    private static func isRule(_ line: String) -> Bool {
        line.count >= 3 && (Set(line) == ["-"] || Set(line) == ["*"] || Set(line) == ["_"])
    }

    private static func isTableSeparator(_ line: String) -> Bool {
        guard line.hasPrefix("|") || line.contains("-") else { return false }
        let allowed = Set("|-: \t")
        return line.contains("-") && line.allSatisfy { allowed.contains($0) }
    }

    private static func cells(of row: String) -> [String] {
        var r = row
        if r.hasPrefix("|") { r.removeFirst() }
        if r.hasSuffix("|") { r.removeLast() }
        return r.components(separatedBy: "|")
            .map { $0.trimmingCharacters(in: .whitespaces) }
    }

    private static func listItem(_ line: String) -> MarkdownListItem? {
        let leading = line.prefix(while: { $0 == " " }).count
        let trimmed = line.trimmingCharacters(in: .whitespaces)
        for bullet in ["- ", "* ", "+ "] {
            if trimmed.hasPrefix(bullet) {
                return MarkdownListItem(
                    indent: leading / 2,
                    marker: "•",
                    text: String(trimmed.dropFirst(bullet.count)))
            }
        }
        let digits = trimmed.prefix(while: \.isNumber)
        if !digits.isEmpty {
            let rest = trimmed.dropFirst(digits.count)
            if rest.hasPrefix(". ") || rest.hasPrefix(") ") {
                return MarkdownListItem(
                    indent: leading / 2,
                    marker: "\(digits).",
                    text: String(rest.dropFirst(2)))
            }
        }
        return nil
    }
}
