export const sameProjectPath = (a: string, b: string) => a.replaceAll("/", "\\").replace(/\\+$/, "").toLowerCase() === b.replaceAll("/", "\\").replace(/\\+$/, "").toLowerCase();

export function mergeRefreshPaths(...groups: string[][]): string[] {
  const paths: string[] = [];
  for (const path of groups.flat()) {
    if (path && !paths.some(existing => sameProjectPath(existing, path))) paths.push(path);
  }
  return paths;
}

export function withoutRefreshPaths(paths: string[], removed: string[]): string[] {
  return paths.filter(path => !removed.some(candidate => sameProjectPath(path, candidate)));
}

export function settleRefreshPaths(current: string[], completed: string[], queued: string[], continueQueued: boolean): string[] {
  return mergeRefreshPaths(withoutRefreshPaths(current, completed), continueQueued ? queued : []);
}
