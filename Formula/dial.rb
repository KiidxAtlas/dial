class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.3.14"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "8f2dfa2e0bac264de751c5daa104157cccd94d27eb4039b2b82ae7748505d989"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "0104ce3da4be0bfa3d8c435faf8a32468a03de44a01f818e79feb7f7ec37720a"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "e885f6f466e8daef0e5cddf879ebfddab1e6780a146160999fb4f8ae120db70e"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "2f424d4108eb8a0baed57546eff59c47b3d3ea4a20d14a6c9547d1b45453ec2b"
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
