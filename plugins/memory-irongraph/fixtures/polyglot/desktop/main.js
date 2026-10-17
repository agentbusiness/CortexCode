const { ipcMain } = require('electron');

function readSetting(key) {
  return process.env.SETTINGS_PREFIX + key;
}

ipcMain.handle('settings:get', async (event, key) => readSetting(key));
