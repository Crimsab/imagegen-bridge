import { describe, expect, test } from "bun:test";
import {
  extractGitHubChanges,
  parseConventionalCommit,
  renderReleaseNotes,
  selectPreviousStableTag,
  type ReleaseCommit,
} from "./release-notes";

const author = {
  authorName: "Crimsab",
  authorEmail: "121881650+Crimsab@users.noreply.github.com",
};

function commit(sha: string, subject: string, body = ""): ReleaseCommit {
  return { sha, subject, body, ...author };
}

describe("automatic release notes", () => {
  test("parses scopes and breaking conventional commits", () => {
    const parsed = parseConventionalCommit(
      commit("a".repeat(40), "feat(api)!: remove legacy field"),
    );
    expect(parsed.type).toBe("feat");
    expect(parsed.scope).toBe("api");
    expect(parsed.description).toBe("remove legacy field");
    expect(parsed.breaking).toBeTrue();
  });

  test("selects the previous stable version and ignores previews", () => {
    expect(
      selectPreviousStableTag(
        ["v0.4.0-preview.2", "v0.3.0", "v0.2.0"],
        "v0.4.0",
      ),
    ).toBe("v0.3.0");
  });

  test("keeps useful GitHub PR notes but removes its duplicate changelog", () => {
    const extracted = extractGitHubChanges(
      "## What's Changed\n### Features\n* Add presets by @contributor in #12\n\n**Full Changelog**: old...new",
    );
    expect(extracted).toContain("#### Features");
    expect(extracted).toContain("#12");
    expect(extracted).not.toContain("Full Changelog");
  });

  test("renders highlights, categorized commits, contributors, comparison and packages", () => {
    const notes = renderReleaseNotes({
      repository: "Crimsab/imagegen-bridge",
      tag: "v0.4.0",
      previousTag: "v0.3.0",
      commits: [
        commit("1".repeat(40), "feat(runtime): fan out all requested images"),
        commit("2".repeat(40), "fix(cli): explain upstream throttling"),
        commit("3".repeat(40), "docs: explain release automation"),
        commit("4".repeat(40), "chore(release): v0.4.0"),
      ],
      githubNotes:
        "## What's Changed\n* Add a preset by @contributor in #12\n\n**Full Changelog**: old...new",
    });
    expect(notes).toContain("## Highlights");
    expect(notes).toContain("### Features");
    expect(notes).toContain("### Fixes");
    expect(notes).toContain("### Documentation");
    expect(notes).toContain("**runtime:**");
    expect(notes).toContain("@Crimsab");
    expect(notes).toContain("@contributor");
    expect(notes).toContain("compare/v0.3.0...v0.4.0");
    expect(notes).toContain("SHA256SUMS");
    expect(notes).toContain("ghcr.io/crimsab/imagegen-bridge:0.4.0");
    expect(notes).not.toContain("chore(release)");
  });
});
