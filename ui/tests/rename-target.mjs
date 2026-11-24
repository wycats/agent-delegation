import { chromium } from 'playwright';
import assert from 'node:assert/strict';

const browser = await chromium.launch();
try {
  const page = await browser.newPage();
  page.setDefaultTimeout(10_000);
  const url = process.env.SETTINGS_TEST_URL;
  const button = (tab, name) => tab.getByRole('button', { name, exact: true });
  const field = (tab, name) => tab.getByLabel(name, { exact: true });
  await page.goto(url);
  await button(page, 'Edit work-neon').click();
  const config = await field(page, 'Config reference').inputValue();
  const receipt = await field(page, 'Receipt reference').inputValue();
  await button(page, 'Cancel').click();

  await button(page, 'Add target').click();
  await field(page, 'Target name').fill('temporary');
  await field(page, 'Backend').selectOption({ label: 'Saved VS Code assignment' });
  await field(page, 'Config reference').fill(config);
  await field(page, 'Receipt reference').fill(receipt);
  await button(page, 'Save target').click();
  await button(page, 'Edit temporary').click();
  assert.equal(await field(page, 'Config reference').inputValue(), config);
  assert.equal(await field(page, 'Receipt reference').inputValue(), receipt);
  await field(page, 'Target name').fill('stale-name');

  // A second editor saves a change to the same target while the first has a draft.
  const other = await browser.newPage();
  await other.goto(url);
  await button(other, 'Edit temporary').click();
  await field(other, 'Backend').selectOption({ label: 'Claude Code' });
  await field(other, 'Model').fill('another-editor-model');
  await button(other, 'Save target').click();
  await button(other, 'Edit temporary').waitFor();
  await other.close();

  await button(page, 'Save target').click();
  await button(page, 'Discard edits and reload').click();
  await button(page, 'Edit temporary').click();
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
