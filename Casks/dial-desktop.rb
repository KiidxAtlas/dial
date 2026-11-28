cask "dial-desktop" do
  arch arm: "aarch64", intel: "x64"

  version "0.4.0"
  sha256 arm:   "ea331ef0448a867b3ec06b0348bb81790360da1704f2fd61d58ff123c62c2a86",
         intel: "849bf548f25e34bb9bb6ba9afe222e5ff25c89a53551dac9907d98d2ac54082c"

  url "https://github.com/KiidxAtlas/dial-desktop/releases/download/v#{version}/Dial_#{arch}.app.tar.gz"
  name "Dial Desktop"
  desc "Native desktop app for Dial, the local-first AI coding agent"
  homepage "https://dial.rootatlas.dev/"

  depends_on macos: ">= :ventura"
  depends_on formula: "kiidxatlas/dial/dial"

  app "Dial.app"

  postflight do
    system_command "/usr/bin/xattr",
                   args: ["-dr", "com.apple.quarantine", "#{appdir}/Dial.app"],
                   must_succeed: false
  end

  zap trash: [
    "~/Library/Application Support/dev.dial.desktop",
    "~/Library/Caches/dev.dial.desktop",
    "~/Library/Logs/dev.dial.desktop",
    "~/Library/Preferences/dev.dial.desktop.plist",
    "~/Library/Saved Application State/dev.dial.desktop.savedState",
  ]

  caveats <<~EOS
    Dial Desktop is currently ad-hoc signed on macOS; quarantine attributes are cleared on install.
  EOS
end
