import { $ } from "bun";

/** Clone a repository and check out the specified branch. */
export async function cloneRepo(
  url: string,
  branch: string,
  workDir: string
): Promise<void> {
  // Configure git credentials from env if available
  const token = process.env.GITHUB_TOKEN;
  if (token) {
    // Use token-based auth for HTTPS URLs
    await $`git config --global credential.helper '!f() { echo "password=${token}"; }; f'`.quiet();
    await $`git config --global user.email "tasks@localhost"`.quiet();
    await $`git config --global user.name "Tasks Agent"`.quiet();
  }

  await $`git clone ${url} ${workDir}`.quiet();
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
