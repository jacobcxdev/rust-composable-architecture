# Check the portable skill package and local links without a Rust build.
require "json"
require "open3"
require "fileutils"
require "tmpdir"
require "yaml"

Dir.chdir(File.expand_path("..", __dir__))

manifest = JSON.parse(File.read("plugins/rca/plugin.json"))
catalog = JSON.parse(File.read(".agents/plugins/marketplace.json"))
abort "Expected the rca@rca-tools install name." unless catalog.fetch("name") == "rca-tools" && manifest.fetch("name") == "rca"
# This plugin ships only skills. A new hook, server or component needs its own
# installation checks before we classify a manifest edit as documentation.
metadata = %w[$schema name version description author repository license]
abort "Unknown plugin component. Review how to test its installation." unless (manifest.keys - metadata).empty?
entries = catalog.fetch("plugins").select { |entry| entry.fetch("name") == "rca" }
abort "The RCA catalog must point to the bundled plugin." unless entries.length == 1 &&
  entries.first.fetch("source") == { "source" => "local", "path" => "./plugins/rca" } &&
  entries.first.fetch("policy").fetch("installation") == "AVAILABLE"

link_check = ["lychee", "--offline", "--include-fragments", "--no-progress"]
%w[rca rca-contribute].each do |name|
  dir = "plugins/rca/skills/#{name}"
  header = File.read("#{dir}/SKILL.md").match(/\A---\r?\n(.+?)\r?\n---(?:\r?\n|\z)/m)
  abort "#{dir}: missing skill frontmatter" unless header
  fields = YAML.safe_load(header[1])
  abort "#{dir}: invalid skill name" unless fields.fetch("name") == name
  description = fields.fetch("description")
  abort "#{dir}: missing trigger description" unless description.is_a?(String) && !description.strip.empty?
  %w[LICENSE-APACHE.md LICENSE-MIT.md].each do |license|
    abort "#{dir}: incorrect #{license}" unless File.binread("#{dir}/#{license}") == File.binread(license)
  end
  Dir.glob("#{dir}/**/*.md").each do |page|
    if File.read(page).match?(%r{github\.com/jacobcxdev/rust-composable-architecture/(?:blob|tree)/[\da-f]{7,40}/}i)
      abort "#{page}: use the installed RCA source or a bundled reference, not a fixed commit link"
    end
  end
  # A relative link into the repository can work on GitHub but break for an
  # application that installs only this skill. Test its installed layout too.
  Dir.mktmpdir("rca-skill-") do |tmp|
    FileUtils.cp_r(dir, tmp)
    files = Dir.glob("#{tmp}/#{name}/**/*.md")
    abort "#{name}: standalone links failed" unless system(*link_check, "--root-dir", "#{tmp}/#{name}", "--", *files)
  end
end

paths, status = Open3.capture2("git", "ls-files", "-z", "--cached", "--others", "--exclude-standard", "--", "*.md")
abort "Could not list the repository documentation." unless status.success?
pages = paths.split("\0").uniq
abort "No Markdown files found." if pages.empty?

# Use the Markdown parser's own link and anchor rules. External sites do not
# decide whether this checkout passes. Release review covers external guidance.
exec(*link_check, "--root-dir", ".", "--", *pages)
