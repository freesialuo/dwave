export function parseRelease(tag, expectedVersion) {
  const match = /^v(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)\.(?:0|[1-9]\d*)(?:-([0-9A-Za-z-]+(?:\.[0-9A-Za-z-]+)*))?$/.exec(tag);
  const prerelease = match?.[1];
  if (!match || match[0] !== tag || prerelease?.split('.').some(identifier => /^0\d+$/.test(identifier))) {
    throw new Error(`Expected a v-prefixed SemVer release tag without build metadata, got ${tag}`);
  }

  const version = tag.slice(1);
  if (version !== expectedVersion) {
    throw new Error(`Tag ${tag} does not match npm package version ${expectedVersion}`);
  }
  return { version, dist_tag: prerelease ? 'next' : 'latest' };
}

export async function publishOnce({ packageVersion, integrity, lookup, publish }) {
  let existing = await lookup();
  if (existing === null) {
    const result = await publish();
    if (result.error) throw result.error;
    if (result.status === 0) return 'published';

    // A failed command can race with another publisher or lose the success response.
    existing = await lookup();
    if (existing === null) {
      throw new Error(`npm publish failed for ${packageVersion} (exit ${result.status ?? result.signal})`);
    }
  }

  if (existing !== integrity) throw new Error(`${packageVersion} already exists with a different tarball`);
  return 'already-published';
}
