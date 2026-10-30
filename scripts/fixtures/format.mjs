// Writes each address as the query a person sends: OpenCage's postal
// templates, through @fragaria/address-formatter, never a format of ours.
// stdin: one {"id", "countryCode", components...} per line.
// stdout: one {"id", "full", "messy"} per line, in the same order.
import { readFileSync } from 'node:fs';
import { createInterface } from 'node:readline';
import addressFormatter from '@fragaria/address-formatter';

// A territory's template ('RE': use_country FR, change_country 'La Réunion,
// France') writes its country text even without appendCountry, so messy is
// formatted under the template it borrows.
const templates = JSON.parse(
  readFileSync(new URL('./node_modules/@fragaria/address-formatter/src/templates/templates.json', import.meta.url)),
);
const borrowed = (code) => templates[code]?.use_country ?? code;

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
  const { postcode, state, state_code, country, countryCode, ...kept } = components;
  const messy = lines({ ...kept, countryCode: borrowed(countryCode) }, { abbreviate: true });
  process.stdout.write(JSON.stringify({ id, full, messy }) + '\n');
}
