import { contextBridge, ipcRenderer } from 'electron';

contextBridge.exposeInMainWorld('willowBridge', {
  getConfig: () => ipcRenderer.invoke('config:get'),
  saveConfig: (config: unknown) => ipcRenderer.invoke('config:set', config),
  getBrand: () => ipcRenderer.invoke('brand:get'),
  getVersion: () => ipcRenderer.invoke('app:version'),
  captureCombo: () => ipcRenderer.invoke('capture:combo'),
  reconnectDiscord: () => ipcRenderer.invoke('rpc:reconnect'),
  openExternal: (url: string) => ipcRenderer.send('app:open-external', url),
  quit: () => ipcRenderer.send('app:quit'),
  onStatus: (callback: (status: unknown) => void) =>
    ipcRenderer.on('status', (_event, status) => callback(status)),
});
