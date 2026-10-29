class Cxporter < Formula
  desc "Direct CLI access to Codex-authenticated MCP servers"
  homepage "https://github.com/mkusaka/cxporter"
  version "__VERSION__"
  license "MIT"

  on_macos do
    if Hardware::CPU.arm?
      url "__DARWIN_ARM64_URL__"
      sha256 "__DARWIN_ARM64_SHA256__"
    else
      url "__DARWIN_X64_URL__"
      sha256 "__DARWIN_X64_SHA256__"
    end
  end

  def install
    bin.install "cxporter"
    (pkgshare/"skills").install Dir["skills/*"]
  end

  def caveats
    <<~EOS
      The bundled agent skill was installed to:
        #{opt_pkgshare}/skills

      Install it with npx skills:
        npx -y skills add "#{opt_pkgshare}/skills" -y --copy
    EOS
  end

  test do
    assert_match version.to_s, shell_output("#{bin}/cxporter --version")
  end
end
