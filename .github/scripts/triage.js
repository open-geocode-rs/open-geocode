// What a pull request still needs before review. Both triage workflows run
// this file from main, so a pull request cannot change how it is judged.
module.exports = function problems({ title, body, labels }) {
  const found = [];

  const kind = /^(fix|feat|perf|refactor|chore|ci|docs|build|test)(\([^)]*\))?!?: \S/.exec(title)?.[1];
  if (!kind) {
    found.push('The title is not a conventional commit, such as `fix(search): a house number with a letter finds its address`.');
  }

  const text = (body ?? '').replace(/<!--[\s\S]*?-->/g, '');
  const sections = new Map();
  let current = null;
  for (const line of text.split(/\r?\n/)) {
    const heading = /^##\s+(.+?)\s*$/.exec(line);
    if (heading) {
      current = heading[1].toLowerCase();
      sections.set(current, []);
    } else if (current) {
      sections.get(current).push(line);
    }
  }
  const section = (name) => (sections.get(name) ?? []).join('\n').trim();

  const impact = ['impact', 'downsides'];
  const required = {
    fix: ['problem', 'change', 'example', 'verification', ...impact],
    feat: ['need', 'change', 'example', 'verification', ...impact],
    perf: ['problem', 'change', 'verification', ...impact],
    refactor: ['reason', 'change', 'behavior', 'verification', ...impact],
  }[kind] ?? ['change', 'verification'];
  for (const name of required) {
    if (!sections.has(name)) {
      found.push(`The **${name}** section is missing. Use the \`${kind ?? 'chore'}\` template from CONTRIBUTING.md.`);
    } else if (!section(name)) {
      found.push(`The **${name}** section is empty.`);
    }
  }

  if (required.includes('impact') && sections.has('impact')) {
    const block = section('impact');
    for (const field of ['Pack', 'Machine', 'Rebuild needed', 'API contract changed']) {
      const value = new RegExp(`^${field}:[ \\t]*(.*)$`, 'mi').exec(block)?.[1]?.trim();
      if (!value) found.push(`Impact: **${field}** is not filled.`);
    }
    for (const row of ['Accuracy', 'Time', 'Space']) {
      const cells = new RegExp(`^\\|\\s*${row}\\s*\\|(.*)$`, 'mi').exec(block)?.[1]
        ?.split('|').map((cell) => cell.trim()).filter((cell, at, all) => at < all.length - 1 || cell);
      if (!cells || cells.length < 2 || cells.some((cell) => !cell)) {
        found.push(`Impact: the **${row}** row needs a value for main and for this PR.`);
      }
    }
  }

  if (labels.includes('experimental') && !/https?:\/\/\S+/.test(text)) {
    found.push('An `experimental` pull request links at least one source: a paper, a journal article, or another geocoder\'s documented design.');
  }

  return found;
};
