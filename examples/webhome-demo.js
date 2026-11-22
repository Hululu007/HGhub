// WebHome 最小演示源：配合 examples/webhome-demo.html 使用。
// 导入方式：插件中心 → 导入与订阅 → 粘贴这份源码（或导入本文件）。
// 导入后点进插件详情页，就能看到“打开 WebHome 首页”。
export const meta = {
  id: "demo.webhome",
  name: "WebHome 演示源",
  version: "1.0.0",
  protocol: 1,
  description: "WebHome 最小演示：搜索、详情、直链播放都走固定条目。",
  webhome: { homePage: "https://cdn.jsdelivr.net/gh/yizu2008/HGhub@main/examples/webhome-demo.html" },
};

const STREAM = "https://test-streams.mux.dev/x36xhzz/x36xhzz.m3u8";

export default function (hg) {
  return {
    webhome: {},
    media: {
      getDetail({ id }) {
        return {
          id,
          title: "演示作品",
          description: "WebHome 演示源的固定条目，播放地址是公开测试流。",
          playlists: [{ id: "main", episodes: [{ id: "ep1", index: 1, title: "第 1 集" }] }],
        };
      },
      resolvePlayback() {
        return { streams: [{ url: STREAM, format: "hls" }] };
      },
    },
    search: {
      search({ query }) {
        return { items: [{ id: "demo-1", title: "演示作品（" + query + "）" }], next: null };
      },
    },
  };
}
