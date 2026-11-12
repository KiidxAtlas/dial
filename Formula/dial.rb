class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.3.5"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "33fb882e50df71bea67523b4edb01158ae8e4b4a1c50c382e6c499302acfc87d"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "e03774d6d7a55c43611ece32aef76b18a668e4ce1ee4bee6049074560578d7ab"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "f0cf5310cee162fd9e6615fe4b23027ce9c30e1aad047209f54cc4cc1c59d97a"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "da6e4047c1784714bcd16b88ff2c52197fdfd918e35ebbd37412291de5b4f9f4"
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
