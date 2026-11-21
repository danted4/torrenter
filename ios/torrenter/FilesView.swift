import SwiftUI

/// A torrent job's files in place of the job list: search, all/none, then
/// the files grouped by folder with a checkbox, size and bar each. Every tick
/// or untick is applied at once (`EngineOwner.setFiles`).
struct FilesView: View {
    @ObservedObject var list: FileList
    /// Actions and the job title only; not observed here.
    let owner: EngineOwner
    let colors: Palette
    @State private var query = ""

    var body: some View {
        if let files = list.files {
            content(files)
        } else {
            Spacer(minLength: 0)
        }
    }

    private func content(_ files: JobFilesView) -> some View {
        let all = files.groups.flatMap(\.files)
        let chosen = Set(all.filter(\.selected).map(\.id))
        let groups: [FileGroupView] = files.groups.compactMap { group in
            let kept = group.files.filter { fileMatches(folder: group.folder, name: $0.name, query: query) }
            return kept.isEmpty ? nil : FileGroupView(folder: group.folder, files: kept)
        }
        let shown = groups.flatMap { $0.files.map(\.id) }
        let allState = checkOf(selected: shown.filter { chosen.contains($0) }.count, total: shown.count)
        let title = owner.jobs.first { $0.id == files.id }?.title ?? ""
        return VStack(alignment: .leading, spacing: 10) {
            HStack(spacing: 8) {
                Text(title)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundColor(colors.fg)
                    .lineLimit(2)
                Spacer(minLength: 8)
                Text("\(chosen.count) of \(all.count)")
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundColor(colors.dim)
            }
            TextField("", text: $query, prompt: Text("search files").foregroundColor(colors.dim))
                .font(.system(size: 14, design: .monospaced))
                .foregroundColor(colors.fg)
                .textInputAutocapitalization(.never)
                .autocorrectionDisabled()
                .padding(.horizontal, 10)
                .padding(.vertical, 8)
                .background(colors.row)
                .overlay(RoundedRectangle(cornerRadius: 2).stroke(colors.dim, lineWidth: 1))
            Button {
                if !shown.isEmpty {
                    owner.setFiles(toggledFiles(chosen, shown, on: allState != .on))
                }
            } label: {
                HStack(spacing: 10) {
                    CheckGlyph(state: allState, colors: colors)
                    Text(query.trimmingCharacters(in: .whitespaces).isEmpty ? "all" : "all shown")
                        .font(.system(size: 13, design: .monospaced))
                        .foregroundColor(colors.dim)
                }
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            hairline
            if groups.isEmpty {
                Text("no files match that search")
                    .font(.system(.body, design: .monospaced))
                    .foregroundColor(colors.dim)
                Spacer(minLength: 0)
            } else {
                ScrollView {
                    LazyVStack(alignment: .leading, spacing: 0) {
                        ForEach(groups, id: \.folder) { group in
                            if !group.folder.isEmpty {
                                folderRow(group, chosen: chosen)
                            }
                            ForEach(group.files, id: \.id) { file in
                                fileRow(file, indented: !group.folder.isEmpty, chosen: chosen)
                            }
                        }
                    }
                    .padding(.bottom, 24)
                }
            }
        }
    }

    private var hairline: some View {
        Rectangle()
            .fill(mix(colors.rowAlt, colors.dim, 0.35))
            .frame(height: 1)
    }

    private func folderRow(_ group: FileGroupView, chosen: Set<UInt32>) -> some View {
        let ids = group.files.map(\.id)
        let on = group.files.filter(\.selected).count
        let state = checkOf(selected: on, total: group.files.count)
        return VStack(spacing: 0) {
            Button {
                owner.setFiles(toggledFiles(chosen, ids, on: state != .on))
            } label: {
                HStack(spacing: 10) {
                    CheckGlyph(state: state, colors: colors)
                    Text(group.folder)
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(colors.fg)
                        .lineLimit(1)
                        .truncationMode(.middle)
                    Spacer(minLength: 8)
                    Text("\(on) of \(group.files.count)")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(colors.dim)
                }
                .padding(.top, 12)
                .padding(.bottom, 8)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            hairline
        }
    }

    private func fileRow(_ file: FileRowView, indented: Bool, chosen: Set<UInt32>) -> some View {
        let fraction = fileFraction(have: file.have, size: file.size)
        let ink = file.selected ? colors.fg : colors.dim
        return VStack(spacing: 0) {
            Button {
                owner.setFiles(toggledFiles(chosen, [file.id], on: !file.selected))
            } label: {
                HStack(spacing: 10) {
                    CheckGlyph(state: file.selected ? .on : .off, colors: colors)
                    VStack(alignment: .leading, spacing: 4) {
                        Text(file.name)
                            .font(.system(size: 13, design: .monospaced))
                            .foregroundColor(ink)
                            .lineLimit(1)
                            .truncationMode(.middle)
                        HStack(spacing: 8) {
                            Text(formatSize(file.size))
                                .font(.system(size: 11, design: .monospaced))
                                .foregroundColor(colors.dim)
                                .frame(width: 96, alignment: .leading)
                            ProgressView(value: fraction)
                                .tint(file.selected ? colors.fg : mix(colors.fg, colors.dim, 0.75))
                            Text("\(Int(fraction * 100))%")
                                .font(.system(size: 11, design: .monospaced))
                                .foregroundColor(colors.dim)
                                .frame(width: 40, alignment: .trailing)
                        }
                    }
                }
                .padding(.leading, indented ? 14 : 0)
                .padding(.vertical, 8)
                .contentShape(Rectangle())
            }
            .buttonStyle(.plain)
            hairline
        }
    }
}

/// A small square checkbox in the theme's ink: a tick when on, a bar for some.
struct CheckGlyph: View {
    let state: Check
    let colors: Palette

    var body: some View {
        ZStack {
            switch state {
            case .on:
                RoundedRectangle(cornerRadius: 3).fill(colors.fg)
                Image(systemName: "checkmark")
                    .font(.system(size: 10, weight: .heavy))
                    .foregroundColor(colors.bg)
            case .some:
                RoundedRectangle(cornerRadius: 3).stroke(colors.fg, lineWidth: 1.5)
                RoundedRectangle(cornerRadius: 1).fill(colors.fg).frame(width: 9, height: 2)
            case .off:
                RoundedRectangle(cornerRadius: 3).stroke(colors.dim, lineWidth: 1.5)
            }
        }
        .frame(width: 18, height: 18)
        .accessibilityValue(state == .on ? "selected" : state == .some ? "partly selected" : "not selected")
    }
}
