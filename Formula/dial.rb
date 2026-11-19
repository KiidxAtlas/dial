class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.3.11"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "cdb4187f5b0d3ead8d3bb35139595d02be607ae491514f2137f31eabfad14ae8"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "a1683fa5e12c688cb6ed3cadf386a08f0fbc83fa94cd99bbcb82dcfd54e21810"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "27547fcb123871934780f82c912364b6a021a06038d64381e2e66123aff58cb6"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "cd2eaa1c646cb108eb90759c29ea2a6db7c97f48b2a38ff3d0bc6b00749fc6bd"
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
