class Lazykatok < Formula
  desc "LazyKatok: local-first KakaoTalk terminal client for Apple Silicon macOS"
  homepage "https://github.com/changeroa/lazykatok"
  url "https://github.com/changeroa/lazykatok.git",
    tag:      "v0.3.2",
    revision: "4b23475efaa3a65c393542cf8799ef9e51af5c1a"
  license "MIT"

  depends_on "rust" => :build
  depends_on arch: :arm64

  def install
    system "cargo", "install", *std_cargo_args
  end

  def caveats
    <<~EOS
      For native KakaoTalk sync, grant your terminal Full Disk Access:
        System Settings > Privacy & Security > Full Disk Access

      Then run:
        lazykatok doctor --json
    EOS
  end

  test do
    assert_match "lazykatok", shell_output("#{bin}/lazykatok --help")
  end
end
