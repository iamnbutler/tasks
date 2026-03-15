import { $ } from "bun";

/** Clone a repository and check out the specified branch. */
export async function cloneRepo(
  url: string,
  branch: string,
  workDir: string
): Promise<void> {
  // Configure git identity
  await $`git config --global user.email "tasks@localhost"`.quiet();
  await $`git config --global user.name "Tasks Agent"`.quiet();

  // Embed token in URL for HTTPS auth if available
  const token = process.env.GITHUB_TOKEN;
  let cloneUrl = url;
  if (token && url.startsWith("https://github.com/")) {
    cloneUrl = url.replace(
      "https://github.com/",
      `https://x-access-token:${token}@github.com/`
    );
  }

  await $`git clone ${cloneUrl} ${workDir}`.quiet();
  await $`git -C ${workDir} checkout -B ${branch}`.quiet();
}

/** Check if a repo already exists at the given path. */
export async function repoExists(workDir: string): Promise<boolean> {
  try {
    await $`git -C ${workDir} rev-parse --git-dir`.quiet();
    return true;
  } catch {
    return false;
  }
}
