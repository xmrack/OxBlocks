#include <cstdio>
#include <cstring>
#include <string>
#include <vector>
#include <iostream>
#include <typeinfo>
#include <boost/variant/get.hpp>
#include "serialization/serialization.h"
#include "serialization/binary_archive.h"
#include "serialization/variant.h"
#include "serialization/string.h"
#include "serialization/containers.h"
#include "serialization/crypto.h"
#include "cryptonote_basic/tx_extra.h"
using namespace cryptonote;
static bool my_parse(const std::vector<uint8_t>& tx_extra, std::vector<tx_extra_field>& f, size_t &consumed)
{
  f.clear(); consumed=0;
  if(tx_extra.empty()) return true;
  binary_archive<false> ar{epee::to_span(tx_extra)};
  do { tx_extra_field field; if(!::do_serialize(ar, field)) return false; f.push_back(field); consumed=ar.getpos(); } while(!ar.eof());
  if(!::serialization::check_stream_state(ar)) return false;
  return true;
}
// Lowercase hex, matching contrib/epee/src/hex.cpp:45 and the explorer's output.
static std::string hx(const std::string &s){ static const char*H="0123456789abcdef"; std::string o; for(unsigned char c: s){o+=H[c>>4];o+=H[c&15];} return o; }
static std::string hxp(const void*p, size_t n){ return hx(std::string((const char*)p,n)); }
int main(){
  std::string line;
  while (std::getline(std::cin, line)) {
    if (line.empty()) { printf("EMPTY\n"); continue; }
    std::vector<uint8_t> extra;
    for (size_t i=0;i+1<line.size();i+=2) extra.push_back((uint8_t)strtol(line.substr(i,2).c_str(),nullptr,16));
    std::vector<tx_extra_field> fields; size_t consumed=0;
    bool ok = my_parse(extra, fields, consumed);
    printf("%s n=%zu consumed=%zu/%zu tags=", ok?"OK":"FAIL", fields.size(), consumed, extra.size());
    // Every decoded VALUE is rendered, not just its tag and length: a rendering
    // that stops at the count cannot tell a correct parser from one that
    // truncates a depth, swaps two keys or drops a nonce byte, which is the
    // only thing this program exists to detect. Padding is the one field with
    // no payload, so its size is the whole of its value.
    for (size_t n=0;n<fields.size();n++){
      if (typeid(tx_extra_padding)==fields[n].type()) printf("P%zu,", boost::get<tx_extra_padding>(fields[n]).size);
      else if (typeid(tx_extra_pub_key)==fields[n].type()) printf("K:%s,", hxp(&boost::get<tx_extra_pub_key>(fields[n]).pub_key,32).c_str());
      else if (typeid(tx_extra_nonce)==fields[n].type()) { auto &s=boost::get<tx_extra_nonce>(fields[n]).nonce; printf("N%zu:%s,", s.size(), hx(s).c_str()); }
      else if (typeid(tx_extra_merge_mining_tag)==fields[n].type()) { auto &m=boost::get<tx_extra_merge_mining_tag>(fields[n]); printf("M:%llu:%s,", (unsigned long long)m.depth, hxp(&m.merkle_root,32).c_str()); }
      else if (typeid(tx_extra_additional_pub_keys)==fields[n].type()) { auto &v=boost::get<tx_extra_additional_pub_keys>(fields[n]).data; printf("A%zu:", v.size()); for(auto&k:v) printf("%s", hxp(&k,32).c_str()); printf(","); }
      else { auto &s=boost::get<tx_extra_mysterious_minergate>(fields[n]).data; printf("G%zu:%s,", s.size(), hx(s).c_str()); }
    }
    printf("\n");
  }
  return 0;
}
