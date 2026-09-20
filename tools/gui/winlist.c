#include <ApplicationServices/ApplicationServices.h>
#include <stdio.h>
int main(void){
  CFArrayRef arr=CGWindowListCopyWindowInfo(kCGWindowListOptionOnScreenOnly|kCGWindowListExcludeDesktopElements,kCGNullWindowID);
  CFIndex n=CFArrayGetCount(arr);
  for(CFIndex i=0;i<n;i++){
    CFDictionaryRef d=CFArrayGetValueAtIndex(arr,i);
    CFStringRef owner=CFDictionaryGetValue(d,kCGWindowOwnerName);
    CFNumberRef layer=CFDictionaryGetValue(d,kCGWindowLayer);
    CFNumberRef pid=CFDictionaryGetValue(d,kCGWindowOwnerPID);
    CFDictionaryRef b=CFDictionaryGetValue(d,kCGWindowBounds);
    CGRect r; CGRectMakeWithDictionaryRepresentation(b,&r);
    int l=0,p=0; if(layer) CFNumberGetValue(layer,kCFNumberIntType,&l); if(pid) CFNumberGetValue(pid,kCFNumberIntType,&p);
    char buf[256]="?"; if(owner) CFStringGetCString(owner,buf,sizeof(buf),kCFStringEncodingUTF8);
    printf("layer=%-4d pid=%-6d %-20s x=%.0f y=%.0f w=%.0f h=%.0f\n",l,p,buf,r.origin.x,r.origin.y,r.size.width,r.size.height);
  }
  CFRelease(arr);
  return 0;
}
