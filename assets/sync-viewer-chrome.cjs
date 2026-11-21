// Embed the dashboard's stylesheet in the two independently deployable, single-file viewers.
'use strict';
const fs = require('node:fs');
const path = require('node:path');
const read = file => fs.readFileSync(path.join(__dirname, file), 'utf8');
const block = /<style id="viewer-chrome">[\s\S]*?<\/style>/g;
function chrome(html) {
  const blocks = html.match(block);
  if (blocks?.length !== 1) throw new Error('expected exactly one viewer-chrome style block');
  if (/<style\b/i.test(html.replace(block, ''))) throw new Error('viewer styles belong in viewer-chrome');
  return blocks[0];
}
function syncedViewer(file) {
  const html = read(file);
  return html.replace(chrome(html), () => `<style id="viewer-chrome">\n${read('viewer-chrome.css')}</style>`);
}
if (require.main === module) {
  for (const file of ['collab-web/index.html', 'share-viewer/index.html']) {
    fs.writeFileSync(path.join(__dirname, file), syncedViewer(file));
  }
}
module.exports = {syncedViewer};
