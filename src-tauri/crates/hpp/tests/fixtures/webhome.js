// Entirely synthetic WebHome source. No real service or credentials.
export const meta = {
  id: "fixture.webhome", name: "虚构 WebHome 源", version: "1.0.0", protocol: 1,
  webhome: { homePage: "https://webhome.example/" },
};
export default function(hg) {
  return {
    media: {
      getDetail({ id }) { return { id, title: "虚构作品", playlists: [{ id: "main", episodes: [{ id: "one", index: 1 }] }] }; },
      resolvePlayback() { return { streams: [{ url: "https://media.example/clip.mp4", format: "progressive" }] }; },
    },
    search: {
      search({ query }) { return { items: [{ id: "hit-1", title: "命中：" + query }], next: null }; },
    },
    webhome: {},
  };
}
