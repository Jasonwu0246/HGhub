// 网页版适配层：替代 @tauri-apps/api/window（浏览器里没有原生窗口控制，全部为空操作）。
const noop = async () => {};
const win = {
  isMaximized: async () => false,
  isFullscreen: async () => false,
  onResized: async () => () => {},
  toggleMaximize: noop,
  minimize: noop,
  close: noop,
  startResizeDragging: noop,
  startDragging: noop,
};
export function getCurrentWindow() { return win; }
