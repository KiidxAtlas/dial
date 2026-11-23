class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.3.16"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "5b8242962d99a024fc78f24738302a63d9a426578d65c51b9713cb499cf6a683"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "1e6b292762123298b5c4f548bd9aa13595b6a17a3adcc7f7ac918171de7e407f"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "26c74c86552f866f070855bb6fae1bed0d131b8e8a5bf0324e27ee662eff2af4"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "1bac7bc17fd979fa58191248ab00782915b95cb36c1586eec45704d4d9f46378"
    end
  end

  def install
    bin.install "dial"
    share.install "LICENSE", "README.md", "THIRD_PARTY_NOTICES", "Silver.LICENSE", "Unscii.LICENSE"
    # Older release archives do not carry both bundled interpreters.
    share.install Dir["LICENSE.node", "LICENSE.python"]
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dial --version")
  end
end
