#include <ApplicationServices/ApplicationServices.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <unistd.h>
#include <wchar.h>
#include <locale.h>

static CGPoint loc(void){ CGEventRef e=CGEventCreate(NULL); CGPoint p=CGEventGetLocation(e); CFRelease(e); return p; }
static void mv(CGPoint p){ CGEventRef e=CGEventCreateMouseEvent(NULL,kCGEventMouseMoved,p,kCGMouseButtonLeft); CGEventPost(kCGHIDEventTap,e); CFRelease(e); }
static void btn(CGPoint p, CGMouseButton b, CGEventType t){ CGEventRef e=CGEventCreateMouseEvent(NULL,t,p,b); CGEventPost(kCGHIDEventTap,e); CFRelease(e); }

static void keycode(int code, CGEventFlags fl){
  CGEventRef d=CGEventCreateKeyboardEvent(NULL,code,true);
  CGEventRef u=CGEventCreateKeyboardEvent(NULL,code,false);
  if(fl){ CGEventSetFlags(d,fl); CGEventSetFlags(u,fl); }
  CGEventPost(kCGHIDEventTap,d); CGEventPost(kCGHIDEventTap,u); CFRelease(d); CFRelease(u);
}

static void typestr(const char*s){
  CFStringRef cs=CFStringCreateWithCString(NULL,s,kCFStringEncodingUTF8);
  CFIndex n=CFStringGetLength(cs);
  UniChar* buf=malloc(sizeof(UniChar)*(n+1));
  CFStringGetCharacters(cs,CFRangeMake(0,n),buf);
  CGEventRef d=CGEventCreateKeyboardEvent(NULL,0,true);
  CGEventKeyboardSetUnicodeString(d,n,buf);
  CGEventPost(kCGHIDEventTap,d);
  CGEventRef u=CGEventCreateKeyboardEvent(NULL,0,false);
  CGEventKeyboardSetUnicodeString(u,n,buf);
  CGEventPost(kCGHIDEventTap,u);
  CFRelease(d); CFRelease(u); CFRelease(cs); free(buf);
}


static void chord(int code, CGEventFlags mod){
  CGEventRef cd=CGEventCreateKeyboardEvent(NULL,55,true); CGEventSetFlags(cd,mod); CGEventPost(kCGHIDEventTap,cd); CFRelease(cd);
  usleep(90000);
  CGEventRef kd=CGEventCreateKeyboardEvent(NULL,code,true); CGEventSetFlags(kd,mod); CGEventPost(kCGHIDEventTap,kd); CFRelease(kd);
  usleep(80000);
  CGEventRef ku=CGEventCreateKeyboardEvent(NULL,code,false); CGEventSetFlags(ku,mod); CGEventPost(kCGHIDEventTap,ku); CFRelease(ku);
  usleep(80000);
  CGEventRef cu=CGEventCreateKeyboardEvent(NULL,55,false); CGEventPost(kCGHIDEventTap,cu); CFRelease(cu);
}
static void appswitch(int n){
  CGEventRef cd=CGEventCreateKeyboardEvent(NULL,55,true); CGEventSetFlags(cd,kCGEventFlagMaskCommand); CGEventPost(kCGHIDEventTap,cd); CFRelease(cd);
  usleep(120000);
  for(int i=0;i<n;i++){
    CGEventRef kd=CGEventCreateKeyboardEvent(NULL,48,true); CGEventSetFlags(kd,kCGEventFlagMaskCommand); CGEventPost(kCGHIDEventTap,kd); CFRelease(kd);
    usleep(120000);
    CGEventRef ku=CGEventCreateKeyboardEvent(NULL,48,false); CGEventSetFlags(ku,kCGEventFlagMaskCommand); CGEventPost(kCGHIDEventTap,ku); CFRelease(ku);
    usleep(120000);
  }
  CGEventRef cu=CGEventCreateKeyboardEvent(NULL,55,false); CGEventPost(kCGHIDEventTap,cu); CFRelease(cu);
}
static void dragto(CGPoint a, CGPoint b){
  mv(a); usleep(250000);
  btn(a,kCGMouseButtonLeft,kCGEventLeftMouseDown); usleep(250000);
  for(int i=1;i<=25;i++){
    CGPoint p={a.x+(b.x-a.x)*i/25.0, a.y+(b.y-a.y)*i/25.0};
    btn(p,kCGMouseButtonLeft,kCGEventLeftMouseDragged); usleep(25000);
  }
  usleep(200000);
  btn(b,kCGMouseButtonLeft,kCGEventLeftMouseUp); usleep(200000);
}

int main(int argc,char**argv){
  setlocale(LC_ALL,"");
  if(argc<2){ fprintf(stderr,"usage: wizact move|click|dclick|type|key|cmdkey|pos ...\n"); return 2; }
  const char* c=argv[1];
  if(!strcmp(c,"pos")){ CGPoint p=loc(); printf("%.0f,%.0f\n",p.x,p.y); return 0; }
  if(!strcmp(c,"move")){ CGPoint p={atof(argv[2]),atof(argv[3])}; mv(p); usleep(120000); CGPoint q=loc(); printf("want %.0f,%.0f got %.0f,%.0f\n",p.x,p.y,q.x,q.y); return 0; }
  if(!strcmp(c,"click")||!strcmp(c,"dclick")){
    CGPoint p={atof(argv[2]),atof(argv[3])};
    mv(p); usleep(180000);
    btn(p,kCGMouseButtonLeft,kCGEventLeftMouseDown); usleep(60000);
    btn(p,kCGMouseButtonLeft,kCGEventLeftMouseUp); usleep(60000);
    if(!strcmp(c,"dclick")){
      btn(p,kCGMouseButtonLeft,kCGEventLeftMouseDown); usleep(60000);
      btn(p,kCGMouseButtonLeft,kCGEventLeftMouseUp);
    }
    usleep(200000); CGPoint q=loc(); printf("clicked want %.0f,%.0f got %.0f,%.0f\n",p.x,p.y,q.x,q.y); return 0;
  }
  if(!strcmp(c,"type")){ typestr(argv[2]); usleep(150000); printf("typed %s\n",argv[2]); return 0; }
  if(!strcmp(c,"key")){ keycode(atoi(argv[2]),0); usleep(150000); printf("key %d\n",atoi(argv[2])); return 0; }
  if(!strcmp(c,"cmdkey")){ /* cmdkey <code> <cmd|shift|both> */
    CGEventFlags f = !strcmp(argv[3],"cmd")?kCGEventFlagMaskCommand : !strcmp(argv[3],"shift")?kCGEventFlagMaskShift : (kCGEventFlagMaskCommand|kCGEventFlagMaskShift);
    keycode(atoi(argv[2]),f); usleep(200000); printf("cmdkey %d %s\n",atoi(argv[2]),argv[3]); return 0;
  }
  if(!strcmp(c,"chord")){ int code=atoi(argv[2]); const char*m=argv[3];
    CGEventFlags f = !strcmp(m,"cmd")?kCGEventFlagMaskCommand : !strcmp(m,"shift")?kCGEventFlagMaskShift :
      !strcmp(m,"cmdshift")?(kCGEventFlagMaskCommand|kCGEventFlagMaskShift) : kCGEventFlagMaskControl;
    chord(code,f); usleep(200000); printf("chord %d %s\n",code,m); return 0; }
  if(!strcmp(c,"appswitch")){ appswitch(atoi(argv[2])); usleep(400000); printf("appswitch %s\n",argv[2]); return 0; }
  if(!strcmp(c,"drag")){ CGPoint a={atof(argv[2]),atof(argv[3])}, b={atof(argv[4]),atof(argv[5])}; dragto(a,b); printf("dragged %.0f,%.0f -> %.0f,%.0f\n",a.x,a.y,b.x,b.y); return 0; }
  if(!strcmp(c,"scroll")){ /* scroll <clicks>：正=向上滚，负=向下滚（WKWebView 页面内容滚动） */
    CGPoint p=loc(); int n=atoi(argv[2]);
    CGEventRef e=CGEventCreateScrollWheelEvent(NULL,kCGScrollEventUnitLine,1,n);
    CGEventPost(kCGHIDEventTap,e); CFRelease(e);
    usleep(300000); CGPoint q=loc(); printf("scrolled %d at %.0f,%.0f\n",n,q.x,q.y); return 0; }
  fprintf(stderr,"unknown cmd %s\n",c); return 2;
}
