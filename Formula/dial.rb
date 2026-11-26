class Dial < Formula
  desc "Local-first coding agent harness"
  homepage "https://github.com/KiidxAtlas/dial"
  version "0.4.0"
  on_macos do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-apple-darwin.tar.gz"
      sha256 "13290c719dd4a5529cd1da1d39c08277b64376cba19bfb0c91b64587222b5bcc"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-apple-darwin.tar.gz"
      sha256 "1938e65fdd3bedf3a62c72f97469aec55fd182ea1b5f57e321a5b06856a5c4e8"
    end
  end
  on_linux do
    on_arm do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-aarch64-unknown-linux-gnu.tar.gz"
      sha256 "8503d8ddbbd1b56a2f7667c8cd4f80cee6c40b3a59f058683d7ea6f503c638f3"
    end
    on_intel do
      url "https://github.com/KiidxAtlas/dial/releases/download/v#{version}/dial-v#{version}-x86_64-unknown-linux-gnu.tar.gz"
      sha256 "d246a882f26d598d77c91e0e1535d8620bb4f60e6dadd9043cf2f44e010151ce"
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
