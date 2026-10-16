/** Remove the Windows extended-length prefix from a path shown in the UI. */
export function displayDirectoryPath(path: string): string {
  const extendedPrefix = "\\\\?\\";
  const extendedUncPrefix = `${extendedPrefix}UNC\\`;

  if (path.slice(0, extendedUncPrefix.length).toUpperCase() === extendedUncPrefix.toUpperCase()) {
    return `\\\\${path.slice(extendedUncPrefix.length)}`;
  }

  return path.startsWith(extendedPrefix) ? path.slice(extendedPrefix.length) : path;
}
