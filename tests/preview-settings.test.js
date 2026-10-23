const test = require('node:test')
const assert = require('node:assert/strict')
const Schema = require('../plugin/core/Schema.js')
test('preview settings can be disabled and renderer overrides stay literal', () => {
  const cfg = Schema.withConfigDefaults({filePreviews: false, previewF3d: '/opt/F3D with spaces/bin/f3d', previewPdfRenderer: 'pdftoppm'})
  assert.equal(cfg.filePreviews, false)
  assert.equal(cfg.previewF3d, '/opt/F3D with spaces/bin/f3d')
  assert.equal(Schema.withConfigDefaults({filePreviews: 'false'}).filePreviews, true)
})
