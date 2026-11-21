export const fmtTime=(value:number)=>{const s=Math.max(0,Math.floor(Number.isFinite(value)?value:0));return `${Math.floor(s/60)}:${(s%60).toString().padStart(2,"0")}`;};
export const formatResolution=(w:number,h:number)=>{const iw=Math.round(w),ih=Math.round(h);return iw>0&&ih>0?`${iw}×${ih}`:"";};
