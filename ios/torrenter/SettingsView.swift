import SwiftUI

/// Settings categories, shown as chips across the top (the desktop sidebar).
enum SettingsTab: CaseIterable, Hashable {
    case appearance
    case downloads
    case accounts

    var label: String {
        switch self {
        case .appearance: "appearance"
        case .downloads: "downloads"
        case .accounts: "debrid accounts"
        }
    }
}

/// A new account being added: provider first, then an optional name and the
/// key (never shown back).
private struct AddAccountForm {
    var service: String?
    var name = ""
    var key = ""
}

/// Full-screen settings in place of the main screen (the header's cog turns
/// into a back arrow). Same layout as the desktop: a category picker, then
/// tiles with the setting on the left and its control on the right.
/// Downloads keep running underneath.
struct SettingsView: View {
    @EnvironmentObject private var owner: EngineOwner
    @Binding var themeId: ThemeId
    @Binding var tab: SettingsTab
    @State private var form: AddAccountForm?

    var colors: Palette { themePalette(themeId) }

    var body: some View {
        VStack(alignment: .leading, spacing: 12) {
            ScrollView(.horizontal, showsIndicators: false) {
                HStack(spacing: 6) {
                    ForEach(SettingsTab.allCases, id: \.self) { each in
                        chip(each.label, on: tab == each) { tab = each }
                    }
                }
            }
            ScrollView {
                VStack(alignment: .leading, spacing: 10) {
                    switch tab {
                    case .appearance: appearance
                    case .downloads: downloads
                    case .accounts: accounts
                    }
                }
                .frame(maxWidth: .infinity, alignment: .leading)
            }
        }
    }

    // MARK: pages

    private var appearance: some View {
        Group {
            pageTitle("appearance")
            tile("theme", "colour theme for every screen") {
                HStack(spacing: 10) {
                    ForEach(ThemeId.allCases, id: \.self) { id in
                        RoundedRectangle(cornerRadius: 3)
                            .fill(themePalette(id).fg)
                            .frame(width: 18, height: 18)
                            .padding(2)
                            .overlay(
                                RoundedRectangle(cornerRadius: 4)
                                    .stroke(colors.fg, lineWidth: themeId == id ? 1.5 : 0)
                            )
                            .contentShape(Rectangle())
                            .onTapGesture {
                                themeId = id
                                Prefs.saveTheme(id)
                            }
                            .accessibilityLabel(id.rawValue)
                    }
                }
            }
        }
    }

    private var downloads: some View {
        Group {
            pageTitle("downloads")
            tile(
                "connections per download",
                "parallel connections for new HTTP and debrid downloads; auto is 4, and each provider caps it",
                stacked: true
            ) {
                HStack(spacing: 4) {
                    chip("auto", on: owner.connections == 0) { owner.connections = 0 }
                    ForEach([1, 2, 4, 8, 16], id: \.self) { parts in
                        chip("\(parts)", on: owner.connections == parts) { owner.connections = parts }
                    }
                }
            }
            tile(
                "remove from debrid account after download",
                "once every file has arrived; only torrents torrenter added"
            ) {
                Toggle("", isOn: $owner.removeAfter)
                    .toggleStyle(MiniSwitch(palette: colors))
                    .labelsHidden()
                    .fixedSize()
            }
            tile(
                "keep alive",
                "keep downloading in the background with a silent audio loop (same switch as the main screen)"
            ) {
                Toggle("", isOn: $owner.keepAliveOn)
                    .toggleStyle(MiniSwitch(palette: colors))
                    .labelsHidden()
                    .fixedSize()
            }
        }
    }

    private var accounts: some View {
        Group {
            HStack {
                pageTitle("debrid accounts")
                Spacer()
                if form == nil {
                    Button("+ add account") { form = AddAccountForm() }
                        .buttonStyle(MonoButton(palette: colors, size: 13))
                }
            }
            if form != nil {
                addForm
            }
            if owner.accounts.isEmpty && form == nil {
                VStack(spacing: 4) {
                    Text("no accounts yet")
                        .font(.system(size: 13, design: .monospaced))
                        .foregroundColor(colors.fg)
                    Text("add one to download magnets through a debrid service instead of from peers")
                        .font(.system(size: 12, design: .monospaced))
                        .foregroundColor(colors.dim)
                        .multilineTextAlignment(.center)
                }
                .padding(.vertical, 18)
                .padding(.horizontal, 12)
                .frame(maxWidth: .infinity)
                .background(card)
            }
            ForEach(owner.accounts) { account in
                accountCard(account)
            }
            Text("uses your own account; the provider's terms apply")
                .font(.system(size: 11, design: .monospaced))
                .foregroundColor(colors.dim)
        }
    }

    private func accountCard(_ account: DebridAccount) -> some View {
        let provider = serviceLabel(account.service)
        let detail = owner.accountNotes[account.id].map { "\(provider) · \($0)" } ?? "\(provider) · key saved"
        return HStack(spacing: 8) {
            VStack(alignment: .leading, spacing: 3) {
                Text(account.name)
                    .font(.system(size: 14, design: .monospaced))
                    .foregroundColor(colors.fg)
                    .lineLimit(1)
                Text(detail)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundColor(colors.dim)
            }
            .frame(maxWidth: .infinity, alignment: .leading)
            Button("test") { owner.testAccount(id: account.id) }
                .buttonStyle(MonoButton(palette: colors, size: 13))
            Button("remove") { owner.removeAccount(id: account.id) }
                .buttonStyle(MonoButton(palette: colors, size: 13, tint: colors.fail))
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(card)
    }

    // MARK: add account

    private var addForm: some View {
        let draft = form ?? AddAccountForm()
        let typed = draft.name.trimmingCharacters(in: .whitespacesAndNewlines)
        let nameError = !typed.isEmpty && owner.nameTaken(typed)
            ? "\"\(typed)\" is already one of your accounts"
            : nil
        let ready = draft.service != nil
            && !draft.key.trimmingCharacters(in: .whitespacesAndNewlines).isEmpty
            && nameError == nil
        return VStack(alignment: .leading, spacing: 10) {
            VStack(alignment: .leading, spacing: 3) {
                Text("add account")
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundColor(colors.fg)
                Text("paste an API key from your provider's account page; it is stored only on this device")
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundColor(colors.dim)
            }
            // The fields sit in an inset panel the header owns.
            VStack(alignment: .leading, spacing: 10) {
                formRow("provider", "the debrid service the key belongs to") {
                    ProviderMenu(service: draft.service, colors: colors) { form?.service = $0 }
                        .equatable()
                }
                if let service = draft.service {
                    Divider().overlay(colors.dim.opacity(0.4))
                    formRow(
                        "name",
                        nameError ?? "shown in the download menu; optional",
                        tint: nameError == nil ? colors.dim : colors.fail
                    ) {
                        field(
                            text: Binding(
                                get: { form?.name ?? "" },
                                set: { form?.name = $0 }
                            ),
                            prompt: owner.defaultName(service: service),
                            secure: false,
                            error: nameError != nil
                        )
                    }
                    Divider().overlay(colors.dim.opacity(0.4))
                    formRow("api key", "from your \(serviceLabel(service)) account settings") {
                        field(
                            text: Binding(
                                get: { form?.key ?? "" },
                                set: { form?.key = $0 }
                            ),
                            prompt: "paste the key",
                            secure: true,
                            error: false
                        )
                    }
                }
            }
            .padding(10)
            .background(RoundedRectangle(cornerRadius: 6).fill(colors.bg.opacity(0.7)))
            // Both actions together, centred: cancel, then the primary save.
            HStack(spacing: 8) {
                Spacer()
                Button("cancel") { form = nil }
                    .buttonStyle(MonoButton(palette: colors, size: 13, tint: colors.fail))
                Button("save account") {
                    guard let service = draft.service else { return }
                    if owner.addAccount(service: service, name: draft.name, key: draft.key) {
                        form = nil
                    }
                }
                .buttonStyle(MonoButton(palette: colors, size: 13, tint: ready ? colors.fg : colors.dim, outlined: ready))
                .disabled(!ready)
                Spacer()
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(card)
    }

    /// A text field styled like the URL field: placeholder centred, typed
    /// text from the left; a fail-coloured border while `error` is set.
    private func field(text: Binding<String>, prompt: String, secure: Bool, error: Bool) -> some View {
        let empty = text.wrappedValue.isEmpty
        let promptText = Text(prompt).foregroundColor(colors.dim)
        return Group {
            if secure {
                SecureField("", text: text, prompt: promptText)
            } else {
                TextField("", text: text, prompt: promptText)
            }
        }
        .multilineTextAlignment(empty ? .center : .leading)
        .font(.system(size: 13, design: .monospaced))
        .foregroundColor(colors.fg)
        .textInputAutocapitalization(.never)
        .autocorrectionDisabled()
        .padding(9)
        .background(colors.bg)
        .overlay(RoundedRectangle(cornerRadius: 2).stroke(error ? colors.fail : colors.dim, lineWidth: 1))
    }

    /// A form row on a phone: title and description, the control under them.
    private func formRow<Control: View>(
        _ title: String,
        _ description: String,
        tint: Color? = nil,
        @ViewBuilder control: () -> Control
    ) -> some View {
        VStack(alignment: .leading, spacing: 6) {
            VStack(alignment: .leading, spacing: 2) {
                Text(title)
                    .font(.system(size: 13, design: .monospaced))
                    .foregroundColor(colors.fg)
                Text(description)
                    .font(.system(size: 11, design: .monospaced))
                    .foregroundColor(tint ?? colors.dim)
            }
            control()
        }
    }

    // MARK: building blocks

    private var card: some View {
        RoundedRectangle(cornerRadius: 4).fill(colors.row)
    }

    private func pageTitle(_ text: String) -> some View {
        Text(text)
            .font(.system(size: 15, design: .monospaced))
            .foregroundColor(colors.fg)
            .padding(.top, 2)
    }

    /// One setting as a full-width tile: title and description on the left,
    /// the control on the right (or under them when `stacked`, for a wide
    /// control on a phone).
    private func tile<Control: View>(
        _ title: String,
        _ description: String,
        stacked: Bool = false,
        @ViewBuilder control: () -> Control
    ) -> some View {
        let text = VStack(alignment: .leading, spacing: 3) {
            Text(title)
                .font(.system(size: 13, design: .monospaced))
                .foregroundColor(colors.fg)
            Text(description)
                .font(.system(size: 11, design: .monospaced))
                .foregroundColor(colors.dim)
                .fixedSize(horizontal: false, vertical: true)
        }
        return Group {
            if stacked {
                VStack(alignment: .leading, spacing: 10) {
                    text
                    HStack {
                        Spacer(minLength: 0)
                        control()
                    }
                }
            } else {
                HStack(alignment: .center, spacing: 12) {
                    text.frame(maxWidth: .infinity, alignment: .leading)
                    control()
                }
            }
        }
        .padding(12)
        .frame(maxWidth: .infinity, alignment: .leading)
        .background(card)
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
}

/// The provider select: full field width, label centred, chevron right.
/// Equatable on what it shows, so the job refresh does not rebuild it mid-tap.
private struct ProviderMenu: View, Equatable {
    let service: String?
    let colors: Palette
    let onPick: (String) -> Void

    static func == (lhs: ProviderMenu, rhs: ProviderMenu) -> Bool {
        lhs.service == rhs.service && lhs.colors == rhs.colors
    }

    var body: some View {
        Menu {
            ForEach(debridServices(), id: \.id) { each in
                Button {
                    onPick(each.id)
                } label: {
                    if service == each.id {
                        Label(each.label, systemImage: "checkmark")
                    } else {
                        Text(each.label)
                    }
                }
            }
        } label: {
            ZStack {
                Text(service.map(serviceLabel) ?? "choose a provider")
                    .lineLimit(1)
                    .padding(.horizontal, 22)
                HStack {
                    Spacer()
                    Image(systemName: "chevron.down")
                        .font(.system(size: 10, weight: .semibold))
                }
                .padding(.trailing, 10)
            }
            .font(.system(size: 13, design: .monospaced))
            .foregroundColor(service == nil ? colors.dim : colors.fg)
            .frame(maxWidth: .infinity, minHeight: 36)
            .overlay(
                RoundedRectangle(cornerRadius: 2)
                    .stroke(service == nil ? colors.dim : colors.fg, lineWidth: 1)
            )
            .contentShape(Rectangle())
        }
    }
}
