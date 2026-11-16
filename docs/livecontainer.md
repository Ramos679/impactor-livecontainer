# Direct LiveContainer installs

This fork adds **Utilities → Install into LiveContainer**. It reads an IPA on
your computer and transfers its app bundle directly into an existing
LiveContainer installation on a connected iPhone or iPad. You do not need to
copy the IPA to Files and import it again on the phone.

## Use the desktop app

1. Install LiveContainer on your iPhone and open it once. Set up its signing
   certificate (JIT-less mode) or the JIT setup required by your apps.
2. Connect the phone by USB, unlock it, and trust this computer. On Windows,
   install Apple's device drivers as required by Impactor.
3. Fully close LiveContainer and any running guest apps.
4. In this fork of Impactor, select your phone, then open **Utilities**.
5. Under **Install into LiveContainer**, click **Find LiveContainer** and select
   the installation you want. Signed bundle-ID suffixes are detected.
6. Click **Choose IPA**, then **Install into LiveContainer**. Wait for completion.
7. Fully close and reopen LiveContainer. Tap the newly added app; LiveContainer
   patches and signs it on its first launch.

The desktop transfer does not need an Apple Account login. LiveContainer still
needs its normal signing or JIT configuration. The feature does not install
LiveContainer itself, bypass iOS signing, enable JIT, or guarantee a particular
guest is compatible. A successful transfer means the guest files were
deployed; it does not mean the guest has launched successfully.

## Command line

```console
plumesign livecontainer --list
plumesign livecontainer --ipa "C:\Games\Game.ipa"
plumesign livecontainer --udid YOUR_DEVICE_UDID --bundle-id com.kdt.livecontainer.YOURTEAM --ipa "Game.ipa"
```

Choose from connected devices when `--udid` is omitted. If multiple
LiveContainer installations are found, the command prompts for one unless
`--bundle-id` is supplied. Use Ctrl+C once to request cancellation and wait
for cleanup.

## Large files and existing apps

- The transfer streams the IPA's decompressed files in 1 MiB chunks. It does
  not extract the full app onto your computer or hold it in memory.
- The phone needs space for the **uncompressed app**, including bundled RomFS
  and shader files. Available storage is checked when the device reports it.
- Every transfer creates a new guest with a unique folder name. It never
  updates or overwrites an existing guest or save. Duplicate versions can be
  managed through LiveContainer after testing the new copy.
- A missing external `romfs.img` remains external. This feature deploys the
  IPA only; separately supplied game data still belongs in that guest's own
  Documents folder.
- Cancelling removes this attempt's temporary folder when the connection is
  working. If the cable is removed or the program is forcibly terminated,
  a hidden `.impactor-stage-...` folder can remain in LiveContainer's Documents;
  it is not a usable guest. The error names the folder when cleanup fails.

## Implementation and compatibility

House Arrest/AFC provides access to the selected app's Documents directory.
Files are written to a unique hidden staging folder; after successful writes,
file-size checks, and ZIP CRC checks, the folder is renamed to
`Documents/Applications/impactor-<uuid>.app`. The main Info.plist is preserved
except for legacy LiveContainer metadata, and exported LiveContainer state is
not carried into a new guest. Relative framework symlinks are validated and
created after regular files. Unsafe paths, case collisions, path traversal,
links escaping the bundle, multiple main apps, and malformed payloads are
rejected before a transfer starts.

This uses LiveContainer's current guest discovery and first-launch patching
behavior, verified against source revision
`b7cfda60` (November 2026). Older versions and third-party forks may differ:

- [Guest discovery](https://github.com/LiveContainer/LiveContainer/blob/main/LiveContainerSwiftUI/LiveContainerSwiftUIApp.swift)
- [First-launch patching and signing](https://github.com/LiveContainer/LiveContainer/blob/main/LiveContainerSwiftUI/Models/LCAppInfo.m)
- [Launching a guest](https://github.com/LiveContainer/LiveContainer/blob/main/LiveContainerSwiftUI/Models/LCAppModel.swift)

LiveContainer's optional import-time compatibility choices are not applied on
the computer. Adjust a guest's compatibility settings in LiveContainer if it
needs them. On-device transfer and launch validation is still required.

## Build and tests

Use the upstream build prerequisites in [.github/CONTRIBUTING.md](../.github/CONTRIBUTING.md).

```console
cargo check -p plume_utils -p plumesign -p plumeimpactor
cargo test -p plume_utils --lib livecontainer
cargo test -p plumesign list_and_deploy_are_distinct_actions
cargo build --release --bin plumeimpactor --bin plumesign
```

This is an independent fork. Upstream Impactor and LiveContainer are not
responsible for this integration. The original project licenses and credits
remain in place.
