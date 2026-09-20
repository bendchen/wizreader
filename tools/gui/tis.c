#include <Carbon/Carbon.h>
#include <stdio.h>
#include <string.h>
static CFStringRef src_id(TISInputSourceRef s){
  CFStringRef id = CFStringCreateCopy(NULL,(CFStringRef)TISGetInputSourceProperty(s,kTISPropertyInputSourceID));
  return id;
}
int main(int argc,char**argv){
  CFArrayRef list = TISCreateInputSourceList(NULL,true);
  CFIndex n = CFArrayGetCount(list);
  if(argc>1 && !strcmp(argv[1],"list")){
    for(CFIndex i=0;i<n;i++){
      TISInputSourceRef s = (TISInputSourceRef)CFArrayGetValueAtIndex(list,i);
      CFStringRef tid = (CFStringRef)TISGetInputSourceProperty(s,kTISPropertyInputSourceID);
      CFBooleanRef sel = (CFBooleanRef)TISGetInputSourceProperty(s,kTISPropertyInputSourceIsSelected);
      char buf[128]="?"; if(tid) CFStringGetCString(tid,buf,sizeof(buf),kCFStringEncodingUTF8);
      printf("%s %s\n", (sel==kCFBooleanTrue)?"*":" ", buf);
    }
    return 0;
  }
  if(argc>2 && !strcmp(argv[1],"select")){
    TISInputSourceRef target=NULL; char buf[128];
    for(CFIndex i=0;i<n;i++){
      TISInputSourceRef s = (TISInputSourceRef)CFArrayGetValueAtIndex(list,i);
      CFStringRef tid=(CFStringRef)TISGetInputSourceProperty(s,kTISPropertyInputSourceID);
      if(!tid) continue; CFStringGetCString(tid,buf,sizeof(buf),kCFStringEncodingUTF8);
      if(strstr(buf,argv[2])){ target=s; break; }
    }
    if(!target){ fprintf(stderr,"not found: %s\n",argv[2]); return 1; }
    OSStatus st = TISSelectInputSource(target);
    printf("select %s -> %d\n", argv[2], (int)st);
    return st==noErr?0:1;
  }
  fprintf(stderr,"usage: tis list | tis select <id-substr>\n"); return 2;
}
