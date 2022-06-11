#pragma once
#include <vector>
#include <string>
#include <future>
#include <unordered_map>
#include <cpr/cpr.h>
#include "Simple-Web-Server/server_http.hpp"
using HttpServer = SimpleWeb::Server<SimpleWeb::HTTP>;

template <typename T>
std::vector<T> join_all_async(std::vector<std::future<T>> &futures)
{
    std::vector<T> result;
    for (auto &future : futures)
    {
        result.push_back(future.get());
    }
    return result;
}

cpr::Header multimap_to_cpr_header(SimpleWeb::CaseInsensitiveMultimap &headers)
{
    cpr::Header h;
    for (auto &header : headers)
    {
        h[header.first] = header.second;
    }
    return h;
}

SimpleWeb::CaseInsensitiveMultimap cpr_header_to_multimap(cpr::Header &headers)
{
    SimpleWeb::CaseInsensitiveMultimap h;
    for (auto &header : headers)
    {
        h.emplace(header.first, header.second);
    }
    return h;
}