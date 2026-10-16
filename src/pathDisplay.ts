/** Remove the Windows extended-length prefix from a path shown in the UI. */
export function displayDirectoryPath(path: string): string {
  return path.startsWith("\\\\?\\") ? path.slice(4) : path;
}
