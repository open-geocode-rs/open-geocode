// Writes each address as the query a person sends: OpenCage's postal
// templates, through @fragaria/address-formatter, never a format of ours.
// stdin: one {"id", "countryCode", components...} per line.
// stdout: one {"id", "full", "messy"} per line, in the same order.
import { createInterface } from 'node:readline';
import addressFormatter from '@fragaria/address-formatter';

const lines = (components, options) =>
  addressFormatter
    .format(components, { output: 'array', ...options })
    // A template joins parts with separators ("city - state"); a dropped part
    // leaves its separator dangling at a line's edge.
    .map((line) => line.trim().replace(/^[-,\s]+|[-,\s]+$/g, ''))
    .filter(Boolean)
    .join(', ');

for await (const line of createInterface({ input: process.stdin })) {
  const { id, ...components } = JSON.parse(line);
  const full = lines(components, { appendCountry: true });
  // Messy drops what people leave out of an address they type and shortens
  // what they shorten, by OpenCage's own abbreviation lists.
  const { postcode, state, state_code, country, ...kept } = components;
  const messy = lines(kept, { abbreviate: true });
  process.stdout.write(JSON.stringify({ id, full, messy }) + '\n');
}
