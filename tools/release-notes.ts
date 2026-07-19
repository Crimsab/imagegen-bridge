import { writeFileSync } from "node:fs";

export type ReleaseCommit = {
  sha: string;
  subject: string;
  body: string;
  authorName: string;
  authorEmail: string;
};

type ParsedCommit = ReleaseCommit & {
  type: string | null;
  scope: string | null;
  description: string;
  breaking: boolean;
};

type ReleaseNotesInput = {
  repository: string;
  tag: string;
  previousTag: string | null;
  commits: ReleaseCommit[];
  githubNotes?: string;
};

const categoryOrder: Array<[string, (commit: ParsedCommit) => boolean]> = [
  ["Breaking changes", (commit) => commit.breaking],
  ["Features", (commit) => !commit.breaking && commit.type === "feat"],
  ["Fixes", (commit) => !commit.breaking && commit.type === "fix"],
  ["Performance", (commit) => !commit.breaking && commit.type === "perf"],
  ["Refactoring", (commit) => !commit.breaking && commit.type === "refactor"],
  ["Documentation", (commit) => !commit.breaking && commit.type === "docs"],
  ["Tests", (commit) => !commit.breaking && commit.type === "test"],
  [
    "CI and build",
    (commit) => !commit.breaking && ["ci", "build"].includes(commit.type ?? ""),
  ],
  [
    "Maintenance",
    (commit) =>
      !commit.breaking && ["chore", "style"].includes(commit.type ?? ""),
  ],
  [
    "Other changes",
    (commit) =>
      !commit.breaking &&
      ![
        "feat",
        "fix",
        "perf",
        "refactor",
        "docs",
        "test",
        "ci",
        "build",
        "chore",
        "style",
      ].includes(commit.type ?? ""),
  ],
];

export function parseConventionalCommit(commit: ReleaseCommit): ParsedCommit {
  const match = commit.subject.match(/^([a-z]+)(?:\(([^)]+)\))?(!)?:\s+(.+)$/i);
  return {
    ...commit,
    type: match?.[1]?.toLowerCase() ?? null,
    scope: match?.[2] ?? null,
    description: match?.[4] ?? commit.subject,
    breaking:
      Boolean(match?.[3]) || /(^|\n)BREAKING[ -]CHANGE:/i.test(commit.body),
  };
}

export function selectPreviousStableTag(
  tags: string[],
  currentTag: string,
): string | null {
  return (
    tags.find((tag) => tag !== currentTag && /^v\d+\.\d+\.\d+$/.test(tag)) ??
    null
  );
}

export function extractGitHubChanges(notes: string): string {
  const lines = notes.replace(/\r\n/g, "\n").split("\n");
  const kept: string[] = [];
  for (const line of lines) {
    if (/^\*\*Full Changelog\*\*:/i.test(line.trim())) continue;
    if (/^## What's Changed\s*$/i.test(line.trim())) continue;
    if (line.startsWith("### ")) kept.push(`#${line}`);
    else if (line.startsWith("## ")) kept.push(`#${line}`);
    else kept.push(line);
  }
  const result = kept.join("\n").trim();
  return /(^|\n)\s*[-*]\s+/m.test(result) ? result : "";
}

export function renderReleaseNotes(input: ReleaseNotesInput): string {
  const commits = input.commits
    .filter((commit) => !/^chore\(release\):/i.test(commit.subject))
    .map(parseConventionalCommit);
  const compareUrl = input.previousTag
    ? `https://github.com/${input.repository}/compare/${input.previousTag}...${input.tag}`
    : `https://github.com/${input.repository}/releases/tag/${input.tag}`;
  const highlights = commits.filter(
    (commit) =>
      commit.breaking || ["feat", "fix", "perf"].includes(commit.type ?? ""),
  );
  const selectedHighlights = (
    highlights.length > 0 ? highlights : commits
  ).slice(0, 6);
  const lines = ["## Highlights", ""];

  if (selectedHighlights.length === 0) {
    lines.push("- Maintenance and release packaging updates.");
  } else {
    lines.push(
      ...selectedHighlights.map(
        (commit) =>
          `- ${capitalize(escapeMarkdown(commit.description))} ([${commit.sha.slice(0, 7)}](${commitUrl(input.repository, commit.sha)}))`,
      ),
    );
  }

  const githubChanges = extractGitHubChanges(input.githubNotes ?? "");
  if (githubChanges) lines.push("", "## Pull requests", "", githubChanges);

  lines.push("", "## Commits", "");
  let renderedCategory = false;
  for (const [title, matches] of categoryOrder) {
    const category = commits.filter(matches);
    if (category.length === 0) continue;
    renderedCategory = true;
    lines.push(`### ${title}`, "");
    lines.push(
      ...category.map((commit) => commitLine(input.repository, commit)),
      "",
    );
  }
  if (!renderedCategory)
    lines.push("- No user-visible commits in this release.", "");

  const contributors = uniqueContributors(
    input.repository,
    commits,
    input.githubNotes ?? "",
  );
  if (contributors.length > 0) {
    lines.push(
      "## Contributors",
      "",
      ...contributors.map((contributor) => `- ${contributor}`),
      "",
    );
  }

  lines.push(
    `**Full Changelog**: ${compareUrl}`,
    "",
    "---",
    "## Downloads",
    "",
    "- `imagegen-bridge-*-linux-x86_64.tar.gz`: Linux x86-64 CLI",
    "- `imagegen-bridge-*-linux-aarch64.tar.gz`: Linux ARM64 CLI",
    "- `imagegen-bridge-*-macos-aarch64.tar.gz`: macOS Apple Silicon CLI",
    "- `imagegen-bridge-*-macos-x86_64.tar.gz`: macOS Intel CLI",
    "- `imagegen-bridge-*-windows-x86_64.zip`: Windows x86-64 CLI",
    "- `SHA256SUMS`: checksums for every release archive",
    "",
    "## Packages",
    "",
    "- [crates.io](https://crates.io/crates/imagegen-bridge-cli)",
    "- [PyPI](https://pypi.org/project/imagegen-bridge/)",
    "- [npm](https://www.npmjs.com/package/imagegen-bridge)",
    `- \`ghcr.io/crimsab/imagegen-bridge:${input.tag.replace(/^v/, "")}\``,
    "",
  );
  return lines.join("\n");
}

function commitLine(repository: string, commit: ParsedCommit): string {
  const scope = commit.scope ? `**${escapeMarkdown(commit.scope)}:** ` : "";
  const author = formatAuthor(repository, commit);
  const byline = author ? ` by ${author}` : "";
  return `- ${scope}${capitalize(escapeMarkdown(commit.description))}${byline} in [${commit.sha.slice(0, 7)}](${commitUrl(repository, commit.sha)})`;
}

function uniqueContributors(
  repository: string,
  commits: ParsedCommit[],
  githubNotes: string,
): string[] {
  const contributors = new Set<string>();
  for (const commit of commits) {
    const author = formatAuthor(repository, commit);
    if (author && !/\[bot\]$/i.test(author)) contributors.add(author);
  }
  for (const match of githubNotes.matchAll(
    /@([A-Za-z0-9](?:[A-Za-z0-9-]{0,38}))/g,
  )) {
    const contributor = `@${match[1]}`;
    if (!/\[bot\]$/i.test(contributor)) contributors.add(contributor);
  }
  return [...contributors].sort((left, right) => left.localeCompare(right));
}

function formatAuthor(repository: string, commit: ReleaseCommit): string {
  const owner = repository.split("/")[0] ?? "";
  const name = commit.authorName.trim();
  if (name.startsWith("@")) return name;
  if (name.toLowerCase() === owner.toLowerCase()) return `@${owner}`;
  const login = commit.authorEmail.match(
    /^(?:\d+\+)?([^@]+)@users\.noreply\.github\.com$/i,
  )?.[1];
  return login ? `@${login}` : escapeMarkdown(name);
}

function capitalize(value: string): string {
  return value.length === 0 ? value : value[0].toUpperCase() + value.slice(1);
}

function escapeMarkdown(value: string): string {
  return value
    .trim()
    .replace(/\s+/g, " ")
    .replace(/([\\`[\]])/g, "\\$1");
}

function commitUrl(repository: string, sha: string): string {
  return `https://github.com/${repository}/commit/${sha}`;
}

function run(command: string[], allowFailure = false): string {
  const result = Bun.spawnSync(command, {
    stdout: "pipe",
    stderr: "pipe",
    env: process.env,
  });
  if (result.exitCode !== 0 && !allowFailure) {
    throw new Error(
      `${command.join(" ")} failed:\n${result.stderr.toString()}`,
    );
  }
  return result.exitCode === 0 ? result.stdout.toString() : "";
}

function readCommits(range: string): ReleaseCommit[] {
  const format = "%H%x1f%s%x1f%b%x1f%an%x1f%ae%x1e";
  return run([
    "git",
    "log",
    "--first-parent",
    "--reverse",
    `--format=${format}`,
    range,
  ])
    .split("\x1e")
    .map((record) => record.trim())
    .filter(Boolean)
    .map((record) => {
      const [
        sha = "",
        subject = "",
        body = "",
        authorName = "",
        authorEmail = "",
      ] = record.split("\x1f");
      return { sha, subject, body, authorName, authorEmail };
    });
}

function findPreviousTag(tag: string): string | null {
  const parent = run(["git", "rev-parse", `${tag}^{}^`], true).trim();
  if (!parent) return null;
  const tags = run(
    ["git", "tag", "--merged", parent, "--sort=-version:refname"],
    true,
  )
    .split("\n")
    .map((candidate) => candidate.trim())
    .filter(Boolean);
  return selectPreviousStableTag(tags, tag);
}

function generateGitHubNotes(
  repository: string,
  tag: string,
  previousTag: string | null,
): string {
  const command = [
    "gh",
    "api",
    `repos/${repository}/releases/generate-notes`,
    "-f",
    `tag_name=${tag}`,
    "--jq",
    ".body",
  ];
  if (previousTag) command.push("-f", `previous_tag_name=${previousTag}`);
  return run(command, true).trim();
}

function parseArguments(argv: string[]): Map<string, string> {
  const argumentsMap = new Map<string, string>();
  for (let index = 0; index < argv.length; index += 1) {
    const argument = argv[index];
    if (!argument?.startsWith("--")) continue;
    const value = argv[index + 1];
    if (value && !value.startsWith("--")) {
      argumentsMap.set(argument.slice(2), value);
      index += 1;
    }
  }
  return argumentsMap;
}

function main(): void {
  const argumentsMap = parseArguments(Bun.argv.slice(2));
  const repository = argumentsMap.get("repo") ?? process.env.GITHUB_REPOSITORY;
  const tag = argumentsMap.get("tag") ?? process.env.GITHUB_REF_NAME;
  const output = argumentsMap.get("out") ?? "release-notes.md";
  if (!repository || !/^[A-Za-z0-9_.-]+\/[A-Za-z0-9_.-]+$/.test(repository))
    throw new Error("Missing or invalid repository");
  if (!tag || !/^v\d+\.\d+\.\d+(?:[-+][0-9A-Za-z.-]+)?$/.test(tag))
    throw new Error("Missing or invalid release tag");

  const previousTag = findPreviousTag(tag);
  const range = previousTag ? `${previousTag}..${tag}` : tag;
  const notes = renderReleaseNotes({
    repository,
    tag,
    previousTag,
    commits: readCommits(range),
    githubNotes: generateGitHubNotes(repository, tag, previousTag),
  });
  writeFileSync(output, notes, "utf8");
  console.log(`Wrote ${output} from ${range}`);
}

if (import.meta.main) main();
