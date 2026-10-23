class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.1.22"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "ea82a78d994b3f058605e1fbf62352178affaec5ceefcde7d48c6e8ee9dd5727"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "1e1698d74abe39e53652c68fb5d9fa09c2567eff9a1c63eeb782aaa634ff1e62"
    end
  end
  on_linux do
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "06e093a4c244e2207395b0a2e25dbc8f77e30d7910cfc6e861bc0e160ce838b1"
    end
  end

  def install
    bin.install "dial"
    share.install "LICENSE", "README.md", "THIRD_PARTY_NOTICES", "Silver.LICENSE", "Unscii.LICENSE"
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/dial --version")
  end
end
