class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.2.0"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "3d4b3fd242e9d1edc3e6ff2f78fcdc7f0a040570902ed7d6d6075328a5aac993"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "232a764484d5f2139d19a1f2334bfe2703a12bd0906fa347b97a594fa95d3778"
    end
  end
  on_linux do
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "33dd91ef37d51dfea752d7d96e602a52e5ed7708cf836217d0d4facfbca4fa95"
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
