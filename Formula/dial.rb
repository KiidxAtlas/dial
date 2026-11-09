class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.3.1"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "54b361baefb26aeff0929f1402db15370269c25cfa822f1b16ee41985ee99f24"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "2f99310d063f8ff42fcf5a1873b42ad5431039744723d3a6b49bc2c3ffcf2712"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "0a44480e55bc58064e11e7bf43126b0122381292dfb82cd0bbee5944d94f82b7"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "a778ae8e74079504774f4e63f82fdadc8cce38f33296c9b43f6af0c7ceaf90e2"
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
