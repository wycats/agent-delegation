import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  page.setDefaultTimeout(10_000);
  const url = process.env.SETTINGS_TEST_URL;
  const button = (tab, name) => tab.getByRole('button', { name, exact: true });
  const field = (tab, name) => tab.getByLabel(name, { exact: true });
  const connection = (tab) => tab.locator('summary').filter({ hasText: 'Connection settings' }).click();
  await page.goto(url);
  await button(page, 'Edit work-neon').click();
  assert.equal(await field(page, 'Chat receipt file').isVisible(), false);
  await connection(page);
  const config = await field(page, 'Assignment config file').inputValue();
  const receipt = await field(page, 'Chat receipt file').inputValue();
  await button(page, 'Cancel').click();

  await button(page, 'Add target').click();
  await field(page, 'Target name').fill('temporary');
  const purpose = field(page, 'What will you use this target for?');
  await purpose.selectOption({ label: 'Message a VS Code chat' });
  await button(page, 'Find open sessions').waitFor();
  assert.equal(await field(page, 'Chat receipt file').count(), 0);
  await purpose.selectOption({ label: 'Run assignments' });
  await field(page, 'Where should assignments run?').selectOption({ label: 'VS Code (existing assignment setup)' });
  await field(page, 'Assignment config file').fill(config);
  await field(page, 'Chat receipt file').fill(receipt);
  await button(page, 'Save target').click();
  await button(page, 'Edit temporary').click();
  await connection(page);
  assert.equal(await field(page, 'Assignment config file').inputValue(), config);
  assert.equal(await field(page, 'Chat receipt file').inputValue(), receipt);
  await field(page, 'Target name').fill('stale-name');

  // A second editor saves a change to the same target while the first has a draft.
  const other = await browser.newPage();
  await other.goto(url);
  await button(other, 'Edit temporary').click();
  await connection(other);
  const host = field(other, 'Where should assignments run?');
  await host.selectOption({ label: 'Claude Code' });
  await field(other, 'Model').fill('another-editor-model');
  await host.selectOption({ label: 'Google Antigravity' });
  assert.equal(await field(other, 'Model').inputValue(), '');
  await host.selectOption({ label: 'Claude Code' });
  assert.equal(await field(other, 'Model').inputValue(), 'another-editor-model');
  await button(other, 'Save target').click();
  await button(other, 'Edit temporary').waitFor();
  await other.close();

  await button(page, 'Save target').click();
  await button(page, 'Discard edits and reload').click();
  await button(page, 'Edit temporary').click();
  await connection(page);
  assert.equal(await field(page, 'Model').inputValue(), 'another-editor-model');
  await button(page, 'Delete target').click();
  await button(page, 'Delete temporary').click();
  await page.getByRole('status').filter({ hasText: 'Deleted temporary.' }).waitFor();

  await button(page, 'Edit work-neon').click();
  await field(page, 'Target name').fill('renamed-neon');
  await button(page, 'Save target').click();
  await button(page, 'Edit renamed-neon').waitFor();
} finally {
  await browser.close();
}
