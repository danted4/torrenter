import SwiftUI

/// Full-screen settings in place of the main screen (the cog turns into back).
/// Downloads keep running underneath.
struct SettingsView: View {
    @EnvironmentObject private var owner: EngineOwner
    @Binding var themeId: ThemeId
    let onBack: () -> Void
    @State private var drafts: [String: String] = [:]

    var colors: Palette { themePalette(themeId) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            HStack(spacing: 10) {
                Button(action: onBack) {
                    Text("← back")
                        .font(.system(.body, design: .monospaced))
                        .foregroundColor(colors.fg)
                }
                Spacer()
                Text("settings")
                    .font(.system(.title3, design: .monospaced))
                    .foregroundColor(colors.fg)
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    heading("appearance")
                    HStack(spacing: 12) {
                        Text("theme")
                            .font(.system(size: 12, design: .monospaced))
                            .foregroundColor(colors.dim)
                        ForEach(ThemeId.allCases, id: \.self) { id in
                            Circle()
                                .fill(themePalette(id).fg)
                                .frame(width: 20, height: 20)
                                .overlay(Circle().stroke(colors.fg, lineWidth: themeId == id ? 2 : 0))
                                .onTapGesture {
                                    themeId = id
                                    Prefs.saveTheme(id)
                                }
                        }
                    }

                    heading("background")
                    Toggle(isOn: $owner.keepAliveOn) {
                        Text("keep alive")
                            .font(.system(size: 13, design: .monospaced))
                            .foregroundColor(owner.keepAliveOn ? colors.fg : colors.dim)
                    }
                    .toggleStyle(MiniSwitch(palette: colors))
                    .fixedSize()

                    heading("debrid accounts")
                    Text("Uses your own account. The provider's terms apply.")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(colors.dim)
                    ForEach(debridServices(), id: \.id) { service in
                        account(service)
                    }

                    heading("downloads")
                    Text("connections per download")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(colors.dim)
                    HStack(spacing: 8) {
                        chip("auto", on: owner.connections == 0) { owner.connections = 0 }
                        ForEach([1, 2, 4, 8, 16], id: \.self) { parts in
                            chip("\(parts)", on: owner.connections == parts) { owner.connections = parts }
                        }
                    }
                    Text("new HTTP and debrid downloads; each service caps it")
                        .font(.system(size: 11, design: .monospaced))
                        .foregroundColor(colors.dim)
                    Toggle(isOn: $owner.removeAfter) {
                        Text("remove from service after download")
                            .font(.system(size: 13, design: .monospaced))
                            .foregroundColor(owner.removeAfter ? colors.fg : colors.dim)
                    }
                    .toggleStyle(MiniSwitch(palette: colors))
                    .fixedSize()
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    private func heading(_ text: String) -> some View {
        VStack(alignment: .leading, spacing: 4) {
            Text(text)
                .font(.system(.body, design: .monospaced))
                .foregroundColor(colors.fg)
                .padding(.top, 10)
            Rectangle().fill(colors.dim).frame(height: 1)
        }
    }

    private func chip(_ text: String, on: Bool, action: @escaping () -> Void) -> some View {
        Button(action: action) {
            Text(text)
                .font(.system(size: 13, design: .monospaced))
                .foregroundColor(on ? colors.fg : colors.dim)
                .padding(.horizontal, 10)
                .padding(.vertical, 5)
                .overlay(
                    RoundedRectangle(cornerRadius: 4).stroke(on ? colors.fg : Color.clear, lineWidth: 1)
                )
        }
        .buttonStyle(.plain)
    }

    private func account(_ service: ServiceView) -> some View {
        let keyed = owner.keyed.contains(service.id)
        let draft = Binding(
            get: { drafts[service.id] ?? "" },
            set: { drafts[service.id] = $0 }
        )
        return VStack(alignment: .leading, spacing: 6) {
            HStack(spacing: 8) {
                Text(service.label)
                    .font(.system(size: 14, design: .monospaced))
                    .foregroundColor(colors.fg)
                Text(keyed ? "key saved" : "no key")
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundColor(keyed ? colors.fg : colors.dim)
            }
            if let note = owner.accountNotes[service.id] {
                Text(note)
                    .font(.system(size: 12, design: .monospaced))
                    .foregroundColor(colors.dim)
            }
            SecureField(
                "",
                text: draft,
                prompt: Text("paste API key").foregroundColor(colors.dim)
            )
            .font(.system(size: 13, design: .monospaced))
            .foregroundColor(colors.fg)
            .padding(8)
            .background(colors.rowAlt)
            .overlay(RoundedRectangle(cornerRadius: 2).stroke(colors.dim, lineWidth: 1))
            .textInputAutocapitalization(.never)
            .autocorrectionDisabled()
            HStack(spacing: 8) {
                if !draft.wrappedValue.trimmingCharacters(in: .whitespaces).isEmpty {
                    Button("save") {
                        owner.saveKey(service: service.id, key: draft.wrappedValue)
                        drafts[service.id] = nil
                    }
                    .buttonStyle(MonoButton(palette: colors))
                }
                if keyed {
                    Button("test") { owner.testAccount(service: service.id) }
                        .buttonStyle(MonoButton(palette: colors))
                    Button("remove") { owner.removeKey(service: service.id) }
                        .buttonStyle(MonoButton(palette: colors))
                }
            }
        }
        .padding(10)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(RoundedRectangle(cornerRadius: 4).fill(colors.row))
    }
}
